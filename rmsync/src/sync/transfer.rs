//! SFTP push/pull transfer primitives used by the sync engine.
//!
//! Spec 12: pull_document, pull_batch, delete_local_document.
//! Spec 13 will add the corresponding push side.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use filetime::{set_file_mtime, FileTime};

use crate::device::connection::{DeviceConnection, RemoteFileInfo};
use crate::sync::engine::{
    ConflictResolution, ConflictWinner, SyncAction, SyncActionType, SyncPlan,
};
use crate::sync::scanner::{
    LocalDocumentSnapshot, LocalManifest, RemoteDocumentSnapshot, RemoteManifest, RAW_SUBDIR,
    XOCHITL_PATH,
};
use crate::sync::state_db::{StateDb, SyncFileState, SyncStatus};
use crate::remarkable::metadata::RemarkableMetadata;

#[derive(Debug, Clone)]
pub struct PullResult {
    pub uuid: String,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub current_file: String,
    pub current_uuid: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Download every file belonging to `uuid` from the reMarkable into
/// `{sync_dir}/raw/`, using atomic `.tmp`-then-rename writes to tolerate
/// connection drops.
pub async fn pull_document(
    conn: &DeviceConnection,
    uuid: &str,
    sync_dir: &Path,
) -> Result<PullResult> {
    let start = Instant::now();
    let raw = sync_dir.join(RAW_SUBDIR);
    tokio::fs::create_dir_all(&raw).await?;

    let remote_files = match list_remote_files_for_uuid(conn, uuid).await {
        Ok(files) => files,
        Err(e) => {
            return Ok(PullResult {
                uuid: uuid.into(),
                files_transferred: 0,
                bytes_transferred: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                success: false,
                error: Some(e.to_string()),
            });
        }
    };

    let mut transferred = 0usize;
    let mut bytes = 0u64;
    let mut first_err: Option<String> = None;

    for f in &remote_files {
        if f.is_dir {
            continue;
        }
        let local_path = match safe_local_path_for(&raw, uuid, &f.path) {
            Some(p) => p,
            None => {
                tracing::warn!("rejecting unsafe remote path for {uuid}: {}", f.path);
                first_err.get_or_insert_with(|| format!("rejected unsafe path: {}", f.path));
                continue;
            }
        };
        if let Some(parent) = local_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                first_err = Some(format!("mkdir {}: {e}", parent.display()));
                continue;
            }
        }
        match download_atomic(conn, &f.path, &local_path, f.mtime).await {
            Ok(n) => {
                transferred += 1;
                bytes += n;
            }
            Err(e) => {
                first_err.get_or_insert_with(|| format!("{}: {e}", f.path));
                cleanup_tmp(&local_path).await;
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(PullResult {
        uuid: uuid.into(),
        files_transferred: transferred,
        bytes_transferred: bytes,
        duration_ms,
        success: first_err.is_none() && !remote_files.is_empty(),
        error: first_err,
    })
}

async fn list_remote_files_for_uuid(
    conn: &DeviceConnection,
    uuid: &str,
) -> Result<Vec<RemoteFileInfo>> {
    let entries = conn
        .list_dir(XOCHITL_PATH)
        .await
        .with_context(|| format!("listing {XOCHITL_PATH}"))?;

    let mut files: Vec<RemoteFileInfo> = entries
        .into_iter()
        .filter(|e| e.name.starts_with(&format!("{uuid}.")))
        .collect();

    let subdir = format!("{XOCHITL_PATH}/{uuid}");
    if let Ok(children) = conn.list_dir(&subdir).await {
        files.extend(children);
    }
    Ok(files)
}

/// Read a document's parent UUID and type back from the metadata on disk.
///
/// Recording every synced item as a parentless `DocumentType` would erase the
/// folder hierarchy from the state database — folders would stop being
/// recognisable as folders, and their children would look like roots.
fn parent_and_type(raw: &Path, uuid: &str) -> (String, String) {
    match RemarkableMetadata::from_file(&raw.join(format!("{uuid}.metadata"))) {
        Ok(md) => (md.parent, md.doc_type),
        Err(e) => {
            tracing::debug!("no readable metadata for {uuid} ({e}); recording as root document");
            (String::new(), "DocumentType".into())
        }
    }
}

/// Validate a uuid-like identifier: hex digits and `-` only, length-bounded.
/// Rejects anything that could escape a path component or pollute globs.
///
/// `pub(crate)` so other modules that build filesystem paths from
/// tablet-supplied identifiers (e.g. the document viewer's page IDs) can
/// reuse this validation instead of trusting the device.
pub(crate) fn is_safe_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid.len() <= 64
        && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Validate that a single path component (name between `/`s) is safe to use
/// as a filesystem path component. Rejects `.`, `..`, empty, `/` embedded,
/// NUL bytes, and leading dots (so adversarial dotfiles can't appear).
pub(crate) fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Resolve an attacker-controlled remote path to a local path beneath `raw`,
/// rejecting anything that tries to escape. Returns `None` on any anomaly so
/// the caller can log and skip. A malicious tablet cannot be trusted to
/// return paths that stay inside `XOCHITL_PATH` — every component is
/// validated.
fn safe_local_path_for(raw: &Path, uuid: &str, remote_path: &str) -> Option<PathBuf> {
    if !is_safe_uuid(uuid) {
        return None;
    }
    let subdir_prefix = format!("{XOCHITL_PATH}/{uuid}/");
    let top_prefix = format!("{XOCHITL_PATH}/");
    let (base, rel) = if let Some(rel) = remote_path.strip_prefix(&subdir_prefix) {
        (raw.join(uuid), rel)
    } else if let Some(rel) = remote_path.strip_prefix(&top_prefix) {
        (raw.to_path_buf(), rel)
    } else {
        return None;
    };
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let mut out = base;
    for component in rel.split('/') {
        if !is_safe_component(component) {
            return None;
        }
        out.push(component);
    }
    // Defence in depth: confirm the resolved path still lies under raw/.
    if !out.starts_with(raw) {
        return None;
    }
    Some(out)
}

async fn download_atomic(
    conn: &DeviceConnection,
    remote_path: &str,
    local_path: &Path,
    remote_mtime: u64,
) -> Result<u64> {
    let tmp = tmp_path(local_path);
    let bytes = conn.read_file(remote_path).await?;
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, local_path).await?;
    let mtime = FileTime::from_unix_time(remote_mtime as i64, 0);
    let _ = set_file_mtime(local_path, mtime);
    Ok(bytes.len() as u64)
}

fn tmp_path(local_path: &Path) -> PathBuf {
    let mut s = local_path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

async fn cleanup_tmp(local_path: &Path) {
    let tmp = tmp_path(local_path);
    if tokio::fs::metadata(&tmp).await.is_ok() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

/// Execute every Pull action in the plan, updating the DB after each.
pub async fn pull_batch<F>(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    sync_dir: &Path,
    db: &StateDb,
    progress_callback: F,
) -> Result<Vec<PullResult>>
where
    F: Fn(TransferProgress) + Send + 'static,
{
    let pulls: Vec<&SyncAction> = plan
        .actions
        .iter()
        .filter(|a| matches!(a.action_type, SyncActionType::Pull))
        .collect();

    let files_total = pulls.len();
    let bytes_total = 0u64;
    let mut results = Vec::with_capacity(files_total);
    let mut bytes_done = 0u64;
    for (i, action) in pulls.iter().enumerate() {
        progress_callback(TransferProgress {
            current_file: action.visible_name.clone(),
            current_uuid: action.uuid.clone(),
            files_done: i,
            files_total,
            bytes_done,
            bytes_total,
        });

        let result = pull_document(conn, &action.uuid, sync_dir).await?;
        if result.success {
            // Scan the freshly pulled files to get the real local hash.
            let local_hash = scan_local_hash_for_uuid(
                &action.uuid,
                &sync_dir.join(crate::sync::scanner::RAW_SUBDIR),
            );
            let now = now_secs();
            let state = match db.get_state(&result.uuid)? {
                Some(mut existing) => {
                    existing.local_hash = local_hash.clone();
                    existing.synced_hash = local_hash;
                    existing.sync_status = SyncStatus::Synced;
                    existing.last_sync_at = Some(now);
                    existing
                }
                None => {
                    let (parent_uuid, doc_type) = parent_and_type(
                        &sync_dir.join(crate::sync::scanner::RAW_SUBDIR),
                        &action.uuid,
                    );
                    SyncFileState {
                        uuid: action.uuid.clone(),
                        visible_name: action.visible_name.clone(),
                        parent_uuid,
                        doc_type,
                        local_hash: local_hash.clone(),
                        remote_hash: local_hash.clone(),
                        synced_hash: local_hash,
                        local_mtime: Some(now),
                        remote_mtime: Some(now),
                        synced_mtime: Some(now),
                        last_sync_at: Some(now),
                        sync_status: SyncStatus::Synced,
                        conflict_info: None,
                    }
                }
            };
            db.upsert_state(&state)?;
        } else if let Some(mut state) = db.get_state(&result.uuid)? {
            state.sync_status = SyncStatus::Error;
            db.upsert_state(&state)?;
        }

        bytes_done += result.bytes_transferred;
        results.push(result);
    }

    progress_callback(TransferProgress {
        current_file: String::new(),
        current_uuid: String::new(),
        files_done: files_total,
        files_total,
        bytes_done,
        bytes_total,
    });
    Ok(results)
}

/// Remove every local file belonging to `uuid` under `{sync_dir}/raw/` and
/// drop the state record.
pub fn delete_local_document(uuid: &str, sync_dir: &Path) -> Result<()> {
    let raw = sync_dir.join(RAW_SUBDIR);
    if !raw.exists() {
        return Ok(());
    }
    let subdir = raw.join(uuid);
    if subdir.is_dir() {
        std::fs::remove_dir_all(&subdir)
            .with_context(|| format!("removing {}", subdir.display()))?;
    }
    for entry in std::fs::read_dir(&raw)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{uuid}.")) {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PushResult {
    pub uuid: String,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub success: bool,
    pub error: Option<String>,
}

/// Upload every local file for `uuid` to the reMarkable. Uses `.tmp` upload
/// + rename so partial uploads can't corrupt the device's view.
pub async fn push_document(
    conn: &DeviceConnection,
    uuid: &str,
    sync_dir: &Path,
) -> Result<PushResult> {
    let start = Instant::now();
    let raw = sync_dir.join(RAW_SUBDIR);
    let meta = raw.join(format!("{uuid}.metadata"));
    if !meta.exists() {
        return Ok(PushResult {
            uuid: uuid.into(),
            files_transferred: 0,
            bytes_transferred: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            success: false,
            error: Some(format!("local metadata missing: {}", meta.display())),
        });
    }

    let local_files = local_files_for_uuid(&raw, uuid)?;
    for (path, rel) in &local_files {
        if let Err(reason) = validate_device_file(rel, &std::fs::read(path)?) {
            tracing::error!(uuid, file = %rel, reason, "refusing to upload a malformed file");
            return Ok(PushResult {
                uuid: uuid.into(),
                files_transferred: 0,
                bytes_transferred: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                success: false,
                error: Some(format!("not uploaded, would risk the device UI: {reason}")),
            });
        }
    }
    tracing::debug!(uuid, files = local_files.len(), "uploading document to device");
    let subdir_remote = format!("{XOCHITL_PATH}/{uuid}");
    if local_files.iter().any(|(_, rel)| rel.starts_with(&format!("{uuid}/"))) {
        let _ = conn.mkdir(&subdir_remote).await;
    }

    // Metadata goes last: xochitl lists a document by its `.metadata`, so a
    // document that fails half-way is never presented as a complete one.
    let ordered = upload_order(&local_files);

    // Phase 1: upload everything beside its destination as `<name>.tmp`. Any
    // failure here removes every temp file and leaves the device untouched.
    let mut staged: Vec<String> = Vec::new();
    let mut bytes = 0u64;
    for (local_path, rel) in &ordered {
        let remote_path = format!("{XOCHITL_PATH}/{rel}");
        match stage_upload(conn, local_path, &remote_path).await {
            Ok(n) => {
                bytes += n;
                staged.push(remote_path);
            }
            Err(e) => {
                let first_err = format!("{remote_path}: {e}");
                for done in staged.iter().chain(std::iter::once(&remote_path)) {
                    let _ = conn.delete_file(&format!("{done}.tmp")).await;
                }
                return Ok(PushResult {
                    uuid: uuid.into(),
                    files_transferred: 0,
                    bytes_transferred: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    success: false,
                    error: Some(first_err),
                });
            }
        }
    }

    // Phase 2: move each staged file into place.
    let mut transferred = 0usize;
    let mut first_err: Option<String> = None;
    for remote_path in &staged {
        match commit_upload(conn, remote_path).await {
            Ok(()) => transferred += 1,
            Err(e) => {
                first_err.get_or_insert_with(|| format!("{remote_path}: {e}"));
                let _ = conn.delete_file(&format!("{remote_path}.tmp")).await;
            }
        }
    }

    Ok(PushResult {
        uuid: uuid.into(),
        files_transferred: transferred,
        bytes_transferred: bytes,
        duration_ms: start.elapsed().as_millis() as u64,
        success: first_err.is_none() && !local_files.is_empty(),
        error: first_err,
    })
}

/// Files in upload order: everything else first, `.metadata` last.
fn upload_order(files: &[(PathBuf, String)]) -> Vec<&(PathBuf, String)> {
    let mut ordered: Vec<_> = files.iter().collect();
    ordered.sort_by_key(|(_, rel)| rel.ends_with(".metadata"));
    ordered
}

/// Upload `local_path` to `<remote_path>.tmp`.
async fn stage_upload(conn: &DeviceConnection, local_path: &Path, remote_path: &str) -> Result<u64> {
    let data = tokio::fs::read(local_path).await?;
    conn.write_file(&format!("{remote_path}.tmp"), &data).await?;
    Ok(data.len() as u64)
}

/// Move `<remote_path>.tmp` onto `remote_path`. SFTP v3 rename refuses to
/// replace an existing file, so when the document is already on the device
/// the old file is removed first; the staged copy is kept until the rename
/// succeeds, so a failure here never loses both versions.
async fn commit_upload(conn: &DeviceConnection, remote_path: &str) -> Result<()> {
    let tmp = format!("{remote_path}.tmp");
    let sftp = conn.sftp()?;
    if sftp.rename(&tmp, remote_path).await.is_ok() {
        return Ok(());
    }
    let _ = conn.delete_file(remote_path).await;
    sftp.rename(&tmp, remote_path)
        .await
        .map_err(|e| anyhow::anyhow!("moving the uploaded file into place: {e}"))
}

pub(crate) fn local_files_for_uuid(raw: &Path, uuid: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(raw)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && name.starts_with(&format!("{uuid}.")) {
            out.push((entry.path(), name));
        }
    }
    let subdir = raw.join(uuid);
    if subdir.is_dir() {
        for entry in std::fs::read_dir(&subdir)? {
            let entry = entry?;
            let fname = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() {
                out.push((entry.path(), format!("{uuid}/{fname}")));
            }
        }
    }
    Ok(out)
}

/// Execute every Push action in the plan, updating the DB after each.
pub async fn push_batch<F>(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    sync_dir: &Path,
    db: &StateDb,
    progress_callback: F,
) -> Result<Vec<PushResult>>
where
    F: Fn(TransferProgress) + Send + 'static,
{
    let pushes: Vec<&SyncAction> = plan
        .actions
        .iter()
        .filter(|a| matches!(a.action_type, SyncActionType::Push))
        .collect();
    let files_total = pushes.len();
    let mut bytes_done = 0u64;
    let mut results = Vec::with_capacity(files_total);
    for (i, action) in pushes.iter().enumerate() {
        progress_callback(TransferProgress {
            current_file: action.visible_name.clone(),
            current_uuid: action.uuid.clone(),
            files_done: i,
            files_total,
            bytes_done,
            bytes_total: 0,
        });
        let result = match push_document(conn, &action.uuid, sync_dir).await {
            Ok(r) => r,
            // One unreadable document must not stop the rest of the batch.
            Err(e) => PushResult {
                uuid: action.uuid.clone(),
                files_transferred: 0,
                bytes_transferred: 0,
                duration_ms: 0,
                success: false,
                error: Some(format!("{e:#}")),
            },
        };
        if result.success {
            let raw = sync_dir.join(crate::sync::scanner::RAW_SUBDIR);
            let local_hash = scan_local_hash_for_uuid(&action.uuid, &raw);
            let parent_and_type = parent_and_type(&raw, &action.uuid);
            let state = state_after_push(
                db.get_state(&result.uuid)?,
                action,
                local_hash,
                parent_and_type,
                now_secs(),
            );
            db.upsert_state(&state)?;
            tracing::info!(
                uuid = %action.uuid,
                name = %action.visible_name,
                files = result.files_transferred,
                bytes = result.bytes_transferred,
                duration_ms = result.duration_ms,
                "pushed document to device"
            );
        } else {
            // The baseline is left untouched, so the local change is still
            // detected and the push is retried on the next sync.
            tracing::error!(
                uuid = %action.uuid,
                name = %action.visible_name,
                error = result.error.as_deref().unwrap_or("unknown"),
                "push failed; will retry on next sync"
            );
            if let Some(mut state) = db.get_state(&result.uuid)? {
                state.sync_status = SyncStatus::Error;
                db.upsert_state(&state)?;
            }
        }
        bytes_done += result.bytes_transferred;
        results.push(result);
    }
    progress_callback(TransferProgress {
        current_file: String::new(),
        current_uuid: String::new(),
        files_done: files_total,
        files_total,
        bytes_done,
        bytes_total: 0,
    });
    Ok(results)
}

/// The state row to record once a document has been uploaded: the device now
/// holds exactly what is on disk, so the local, remote and synced hashes all
/// agree and the next diff sees the document as unchanged.
fn state_after_push(
    existing: Option<SyncFileState>,
    action: &SyncAction,
    local_hash: Option<String>,
    (parent_uuid, doc_type): (String, String),
    now: u64,
) -> SyncFileState {
    let mut state = existing.unwrap_or_else(|| SyncFileState {
        uuid: action.uuid.clone(),
        visible_name: action.visible_name.clone(),
        parent_uuid,
        doc_type,
        local_hash: None,
        remote_hash: None,
        synced_hash: None,
        local_mtime: None,
        remote_mtime: None,
        synced_mtime: None,
        last_sync_at: None,
        sync_status: SyncStatus::Synced,
        conflict_info: None,
    });
    state.local_hash = local_hash.clone();
    state.remote_hash = local_hash.clone();
    state.synced_hash = local_hash;
    state.local_mtime = Some(now);
    state.remote_mtime = Some(now);
    state.synced_mtime = Some(now);
    state.sync_status = SyncStatus::Synced;
    state.last_sync_at = Some(now);
    state.conflict_info = None;
    state
}

/// Delete every remote file for `uuid` on the reMarkable.
pub async fn delete_remote_document(conn: &DeviceConnection, uuid: &str) -> Result<()> {
    let entries = conn.list_dir(XOCHITL_PATH).await?;
    let subdir = format!("{XOCHITL_PATH}/{uuid}");
    if let Ok(children) = conn.list_dir(&subdir).await {
        for child in children {
            let _ = conn.delete_file(&child.path).await;
        }
        let _ = conn
            .sftp()?
            .remove_dir(&subdir)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()));
    }
    for e in entries {
        if e.name.starts_with(&format!("{uuid}.")) {
            let _ = conn.delete_file(&e.path).await;
        }
    }
    // Individual deletes above are best-effort; confirm the document is really
    // gone so the caller never records a delete that did not happen (which
    // would let the next sync pull the document straight back).
    let left: Vec<String> = conn
        .list_dir(XOCHITL_PATH)
        .await?
        .into_iter()
        .filter(|e| e.name == uuid || e.name.starts_with(&format!("{uuid}.")))
        .map(|e| e.name)
        .collect();
    if !left.is_empty() {
        anyhow::bail!("{} item(s) for {uuid} are still on the device: {}", left.len(), left.join(", "));
    }
    Ok(())
}

/// Restart the tablet's UI service so documents written to disk appear.
/// This restarts only `xochitl`, never the device, and ends whatever the user
/// was doing on screen, so call it once per sync and only after something was
/// actually written (see `engine::should_restart_ui`).
pub async fn reload_xochitl(conn: &mut DeviceConnection) -> Result<()> {
    let status = conn
        .exec(
            "systemctl restart xochitl",
            "documents were written to the device and its UI must reload to show them",
        )
        .await?;
    if status != 0 {
        tracing::warn!("systemctl restart xochitl exited with {status}");
    }
    Ok(())
}

/// Check a file about to be uploaded so a malformed one cannot be handed to
/// the device UI. `.metadata` and `.content` must be JSON objects, and
/// metadata must carry the fields xochitl needs to list the document.
pub(crate) fn validate_device_file(rel: &str, bytes: &[u8]) -> std::result::Result<(), String> {
    let is_metadata = rel.ends_with(".metadata");
    if !is_metadata && !rel.ends_with(".content") {
        return Ok(());
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("{rel} is not valid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err(format!("{rel} is not a JSON object"));
    };
    if is_metadata {
        for field in ["visibleName", "type"] {
            if !obj.get(field).is_some_and(|v| v.is_string()) {
                return Err(format!("{rel} has no string \"{field}\""));
            }
        }
    }
    Ok(())
}

/// Compute a local content hash for a single UUID after pull/push. Falls
/// back to an empty string if the files can't be read — the next sync will
/// detect the mismatch and re-classify.
pub(crate) fn scan_local_hash_for_uuid(uuid: &str, raw: &Path) -> Option<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(raw) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{uuid}.")) {
                if let Ok(info) = local_file_info(&entry.path(), &name) {
                    files.push(info);
                }
            }
        }
    }
    let subdir = raw.join(uuid);
    if subdir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&subdir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Ok(info) = local_file_info(&entry.path(), &name) {
                    files.push(info);
                }
            }
        }
    }
    crate::sync::scanner::compute_local_hash(uuid, &files, raw).ok()
}

fn local_file_info(
    path: &Path,
    name: &str,
) -> Result<crate::sync::scanner::LocalFileInfo> {
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(crate::sync::scanner::LocalFileInfo {
        path: path.to_path_buf(),
        name: name.to_string(),
        size: md.len(),
        mtime,
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// =========================================================================
// Conflict resolution execution (spec 14)
// =========================================================================

/// Execute a resolved two-sided change. The newer side overwrites the older
/// one in place under the same UUID, after the older version has been saved
/// under `.rmsync/backups/` — no second document is created.
pub async fn execute_conflict_resolution(
    conn: &DeviceConnection,
    resolution: &ConflictResolution,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
    sync_dir: &Path,
    db: &StateDb,
) -> Result<()> {
    if !is_safe_uuid(&resolution.uuid) {
        anyhow::bail!("refusing to resolve conflict for unsafe uuid {}", resolution.uuid);
    }
    let raw = sync_dir.join(RAW_SUBDIR);
    tokio::fs::create_dir_all(&raw).await?;
    let backup_dir = backup_root(sync_dir).join(&resolution.backup_subdir);
    match resolution.winner {
        ConflictWinner::Remote => {
            // Desktop version loses: keep it, then take the device's.
            copy_local_to_backup(&raw, &resolution.uuid, &backup_dir)?;
            let pulled = pull_document(conn, &resolution.uuid, sync_dir).await?;
            if !pulled.success {
                anyhow::bail!(
                    "pulling the newer device version failed: {}",
                    pulled.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
        ConflictWinner::Local => {
            // Device version loses: keep it, then overwrite it with ours.
            download_remote_to_backup(conn, &resolution.uuid, &raw, &backup_dir).await?;
            let pushed = push_document(conn, &resolution.uuid, sync_dir).await?;
            if !pushed.success {
                anyhow::bail!(
                    "uploading the newer desktop version failed: {}",
                    pushed.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
    }
    record_winner_state(db, &resolution.uuid, local, remote, resolution.winner.clone())?;
    tracing::info!(
        uuid = %resolution.uuid,
        winner = ?resolution.winner,
        backup = %backup_dir.display(),
        "overwritten version saved as backup"
    );
    Ok(())
}

/// Copy `uuid`'s local files into `backup_dir`, then remove them from `raw/`.
/// If the backup fails nothing is removed.
pub fn backup_and_delete_local_document(uuid: &str, sync_dir: &Path, backup_dir: &Path) -> Result<()> {
    if !is_safe_uuid(uuid) {
        anyhow::bail!("refusing to delete unsafe uuid {uuid:?}");
    }
    copy_local_to_backup(&sync_dir.join(RAW_SUBDIR), uuid, backup_dir)?;
    delete_local_document(uuid, sync_dir)
}

/// Download the device's copy of `uuid` into `backup_dir`, then delete it from
/// the device. If the backup fails nothing is deleted.
pub async fn backup_and_delete_remote_document(
    conn: &DeviceConnection,
    uuid: &str,
    sync_dir: &Path,
    backup_dir: &Path,
) -> Result<()> {
    if !is_safe_uuid(uuid) {
        anyhow::bail!("refusing to delete unsafe uuid {uuid:?}");
    }
    download_remote_to_backup(conn, uuid, &sync_dir.join(RAW_SUBDIR), backup_dir).await?;
    delete_remote_document(conn, uuid).await
}

/// Root of all conflict backups. Lives beside the state DB, outside `raw/`.
pub fn backup_root(sync_dir: &Path) -> PathBuf {
    sync_dir.join(".rmsync").join("backups")
}

/// Copy every local file for `uuid` into `backup_dir`, keeping the names and
/// layout they have under `raw/`.
pub(crate) fn copy_local_to_backup(raw: &Path, uuid: &str, backup_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(backup_dir)?;
    for (path, rel) in local_files_for_uuid(raw, uuid)? {
        let dest = backup_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&path, &dest)
            .with_context(|| format!("backing up {}", path.display()))?;
    }
    Ok(())
}

/// Download the device's current copy of `uuid` into `backup_dir`, laid out as
/// it would be under `raw/`. Remote paths are validated like any pull.
async fn download_remote_to_backup(
    conn: &DeviceConnection,
    uuid: &str,
    raw: &Path,
    backup_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(backup_dir)?;
    for f in list_remote_files_for_uuid(conn, uuid).await? {
        if f.is_dir {
            continue;
        }
        let Some(safe) = safe_local_path_for(raw, uuid, &f.path) else {
            tracing::warn!(path = %f.path, "rejecting unsafe remote path while backing up");
            continue;
        };
        let rel = safe.strip_prefix(raw).expect("safe_local_path_for guarantees prefix");
        let dest = backup_dir.join(rel);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = conn.read_file(&f.path).await?;
        tokio::fs::write(&dest, &bytes).await?;
    }
    Ok(())
}

/// Measure the device clock minus the desktop clock, in seconds.
///
/// Writes a throwaway file in the device's `/tmp` and reads back the mtime the
/// device stamped on it, which is the device clock's "now". On failure the
/// error says which step failed; callers fall back to an offset of 0 (times
/// compared as-is) and should say so.
pub async fn estimate_clock_offset(conn: &DeviceConnection) -> Result<i64> {
    const PROBE: &str = "/tmp/rmsync-clock-probe";
    let before = now_secs();
    conn.write_file(PROBE, b"")
        .await
        .context("writing the probe file to the device's /tmp")?;
    let listed = conn.list_dir("/tmp").await;
    let _ = conn.delete_file(PROBE).await;
    let device_now = listed
        .context("listing the device's /tmp")?
        .into_iter()
        .find(|f| f.path == PROBE)
        .map(|f| f.mtime)
        .context("probe file missing from the device's /tmp listing")?;
    let offset = clock_offset(device_now, before, now_secs());
    tracing::debug!(device_now, offset_secs = offset, "measured device clock offset");
    Ok(offset)
}

/// Offset given the device's stamp and the desktop clock either side of the
/// probe; the midpoint absorbs the round-trip.
fn clock_offset(device_now: u64, local_before: u64, local_after: u64) -> i64 {
    device_now as i64 - ((local_before + local_after) / 2) as i64
}

fn record_winner_state(
    db: &StateDb,
    uuid: &str,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
    winner: ConflictWinner,
) -> Result<()> {
    let winning_hash = match winner {
        ConflictWinner::Local => local.content_hash.clone(),
        ConflictWinner::Remote => remote.content_hash.clone(),
    };
    let winning_mtime = match winner {
        ConflictWinner::Local => local.mtime,
        ConflictWinner::Remote => remote.mtime,
    };
    let state = SyncFileState {
        uuid: uuid.to_string(),
        visible_name: local.metadata.visible_name.clone(),
        parent_uuid: local.metadata.parent.clone(),
        doc_type: local.metadata.doc_type.clone(),
        local_hash: Some(winning_hash.clone()),
        remote_hash: Some(winning_hash.clone()),
        synced_hash: Some(winning_hash),
        local_mtime: Some(winning_mtime),
        remote_mtime: Some(winning_mtime),
        synced_mtime: Some(winning_mtime),
        last_sync_at: Some(now_secs()),
        sync_status: SyncStatus::Synced,
        conflict_info: None,
    };
    db.upsert_state(&state)?;
    Ok(())
}

/// Resolve every Conflict action in a plan (newest modification wins). A
/// document that fails to resolve is reported and left for the next sync; it
/// does not stop the others.
pub async fn resolve_all_conflicts(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    local_manifest: &LocalManifest,
    remote_manifest: &RemoteManifest,
    sync_dir: &Path,
    db: &StateDb,
    clock_offset: i64,
) -> (Vec<ConflictResolution>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut errors = Vec::new();
    let local_map: std::collections::HashMap<&str, &LocalDocumentSnapshot> = local_manifest
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    let remote_map: std::collections::HashMap<&str, &RemoteDocumentSnapshot> = remote_manifest
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    for action in plan.conflicts() {
        let (Some(l), Some(r)) = (
            local_map.get(action.uuid.as_str()),
            remote_map.get(action.uuid.as_str()),
        ) else {
            continue;
        };
        let resolution = crate::sync::engine::resolve_conflict(action, l, r, clock_offset);
        match execute_conflict_resolution(conn, &resolution, l, r, sync_dir, db).await {
            Ok(()) => resolved.push(resolution),
            Err(e) => {
                tracing::error!(uuid = %action.uuid, error = format!("{e:#}"), "conflict resolution failed");
                errors.push(format!("resolve {}: {e}", action.uuid));
            }
        }
    }
    (resolved, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn safe_local_path_rejects_parent_traversal_in_rel() {
        let raw = Path::new("/sync/raw");
        let malicious = format!("{XOCHITL_PATH}/../../../etc/passwd");
        assert!(safe_local_path_for(raw, "abc", &malicious).is_none());
    }

    #[test]
    fn safe_local_path_rejects_dotdot_component_after_uuid() {
        let raw = Path::new("/sync/raw");
        let malicious = format!("{XOCHITL_PATH}/abc/../../../../etc/passwd");
        assert!(safe_local_path_for(raw, "abc", &malicious).is_none());
    }

    #[test]
    fn safe_local_path_rejects_absolute_foreign_path() {
        let raw = Path::new("/sync/raw");
        assert!(safe_local_path_for(raw, "abc", "/etc/passwd").is_none());
    }

    #[test]
    fn safe_local_path_rejects_dotfile_component() {
        let raw = Path::new("/sync/raw");
        let malicious = format!("{XOCHITL_PATH}/.ssh/authorized_keys");
        assert!(safe_local_path_for(raw, "abc", &malicious).is_none());
    }

    #[test]
    fn safe_local_path_rejects_nul_byte() {
        let raw = Path::new("/sync/raw");
        let malicious = format!("{XOCHITL_PATH}/abc/evil\0name");
        assert!(safe_local_path_for(raw, "abc", &malicious).is_none());
    }

    #[test]
    fn safe_local_path_rejects_non_hex_uuid() {
        let raw = Path::new("/sync/raw");
        let p = format!("{XOCHITL_PATH}/../evil.metadata");
        assert!(safe_local_path_for(raw, "../evil", &p).is_none());
    }

    #[test]
    fn safe_local_path_accepts_legitimate_top_level_file() {
        let raw = Path::new("/sync/raw");
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let p = format!("{XOCHITL_PATH}/{uuid}.metadata");
        assert_eq!(
            safe_local_path_for(raw, uuid, &p),
            Some(PathBuf::from(format!("/sync/raw/{uuid}.metadata")))
        );
    }

    #[test]
    fn safe_local_path_accepts_legitimate_subdir_page() {
        let raw = Path::new("/sync/raw");
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let p = format!("{XOCHITL_PATH}/{uuid}/p1.rm");
        assert_eq!(
            safe_local_path_for(raw, uuid, &p),
            Some(PathBuf::from(format!("/sync/raw/{uuid}/p1.rm")))
        );
    }

    #[test]
    fn is_safe_uuid_accepts_hex_dash_rejects_other() {
        assert!(is_safe_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_uuid("abc123"));
        assert!(!is_safe_uuid(""));
        assert!(!is_safe_uuid(".."));
        assert!(!is_safe_uuid("../evil"));
        assert!(!is_safe_uuid("abc/def"));
        assert!(!is_safe_uuid("abc.def"));
    }

    #[test]
    fn tmp_path_appends_suffix() {
        let p = tmp_path(Path::new("/a/b/c.rm"));
        assert_eq!(p, PathBuf::from("/a/b/c.rm.tmp"));
    }

    #[test]
    fn delete_local_removes_files_and_subdir() {
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        std::fs::create_dir_all(raw.join("abc")).unwrap();
        std::fs::write(raw.join("abc.metadata"), b"x").unwrap();
        std::fs::write(raw.join("abc.content"), b"y").unwrap();
        std::fs::write(raw.join("abc/p1.rm"), b"z").unwrap();
        std::fs::write(raw.join("other.metadata"), b"other").unwrap();

        delete_local_document("abc", dir.path()).unwrap();
        assert!(!raw.join("abc.metadata").exists());
        assert!(!raw.join("abc.content").exists());
        assert!(!raw.join("abc").exists());
        assert!(raw.join("other.metadata").exists());
    }

    #[test]
    fn delete_local_is_noop_when_raw_missing() {
        let dir = tempdir().unwrap();
        delete_local_document("abc", dir.path()).unwrap();
    }

    #[test]
    fn pull_result_construction() {
        let r = PullResult {
            uuid: "a".into(),
            files_transferred: 2,
            bytes_transferred: 100,
            duration_ms: 50,
            success: true,
            error: None,
        };
        assert!(r.success);
        assert_eq!(r.files_transferred, 2);
    }

    #[test]
    fn transfer_progress_fields() {
        let p = TransferProgress {
            current_file: "f".into(),
            current_uuid: "u".into(),
            files_done: 1,
            files_total: 3,
            bytes_done: 100,
            bytes_total: 300,
        };
        assert_eq!(p.files_total, 3);
        assert_eq!(p.bytes_done, 100);
    }

    #[test]
    fn local_files_for_uuid_collects_siblings_and_pages() {
        let dir = tempdir().unwrap();
        let raw = dir.path();
        std::fs::write(raw.join("abc.metadata"), b"m").unwrap();
        std::fs::write(raw.join("abc.content"), b"c").unwrap();
        std::fs::write(raw.join("other.metadata"), b"o").unwrap();
        std::fs::create_dir_all(raw.join("abc")).unwrap();
        std::fs::write(raw.join("abc/p1.rm"), b"1").unwrap();
        std::fs::write(raw.join("abc/p2.rm"), b"2").unwrap();

        let files = local_files_for_uuid(raw, "abc").unwrap();
        let rels: Vec<&String> = files.iter().map(|(_, r)| r).collect();
        assert!(rels.contains(&&"abc.metadata".to_string()));
        assert!(rels.contains(&&"abc.content".to_string()));
        assert!(rels.iter().any(|r| r.ends_with("abc/p1.rm")));
        assert!(rels.iter().any(|r| r.ends_with("abc/p2.rm")));
        assert!(!rels.contains(&&"other.metadata".to_string()));
    }

    #[test]
    fn push_result_construction() {
        let r = PushResult {
            uuid: "a".into(),
            files_transferred: 1,
            bytes_transferred: 10,
            duration_ms: 5,
            success: true,
            error: None,
        };
        assert!(r.success);
    }

    #[tokio::test]
    async fn cleanup_tmp_removes_existing_tmp() {
        let dir = tempdir().unwrap();
        let local = dir.path().join("x.rm");
        let tmp = tmp_path(&local);
        tokio::fs::write(&tmp, b"partial").await.unwrap();
        cleanup_tmp(&local).await;
        assert!(!tmp.exists());
    }

    #[test]
    fn local_backup_preserves_every_byte_and_leaves_the_original_alone() {
        // When the device wins, the desktop copy is only ever overwritten after
        // this backup. A bug here silently destroys work.
        let dir = tempdir().unwrap();
        let raw = dir.path().join("raw");
        std::fs::create_dir_all(raw.join("abc")).unwrap();
        std::fs::write(raw.join("abc.metadata"), b"meta-bytes").unwrap();
        std::fs::write(raw.join("abc.content"), b"content-bytes").unwrap();
        std::fs::write(raw.join("abc/p1.rm"), b"page-1-bytes").unwrap();
        std::fs::write(raw.join("other.metadata"), b"untouched").unwrap();
        let backup = backup_root(dir.path()).join("abc/2026-10-14-desktop-1");

        copy_local_to_backup(&raw, "abc", &backup).unwrap();

        assert_eq!(std::fs::read(backup.join("abc.metadata")).unwrap(), b"meta-bytes");
        assert_eq!(std::fs::read(backup.join("abc.content")).unwrap(), b"content-bytes");
        assert_eq!(std::fs::read(backup.join("abc/p1.rm")).unwrap(), b"page-1-bytes");
        // Other documents are not swept in, and originals are untouched.
        assert!(!backup.join("other.metadata").exists());
        assert_eq!(std::fs::read(raw.join("abc/p1.rm")).unwrap(), b"page-1-bytes");
        // A backup is not a document: nothing new appears under raw/.
        assert_eq!(std::fs::read_dir(&raw).unwrap().count(), 4);
    }

    #[test]
    fn clock_offset_uses_the_probe_midpoint() {
        assert_eq!(clock_offset(1000, 1000, 1000), 0);
        assert_eq!(clock_offset(4600, 1000, 1002), 3599); // device ~1h fast
        assert_eq!(clock_offset(0, 3600, 3600), -3600); // device ~1h slow
    }

    fn push_action(uuid: &str) -> SyncAction {
        SyncAction {
            uuid: uuid.into(),
            visible_name: "Notes".into(),
            action_type: SyncActionType::Push,
            priority: 1,
        }
    }

    fn baseline(uuid: &str, hash: &str) -> SyncFileState {
        SyncFileState {
            uuid: uuid.into(),
            visible_name: "Notes".into(),
            parent_uuid: String::new(),
            doc_type: "DocumentType".into(),
            local_hash: Some("old".into()),
            remote_hash: Some(hash.into()),
            synced_hash: Some(hash.into()),
            local_mtime: Some(1),
            remote_mtime: Some(1),
            synced_mtime: Some(1),
            last_sync_at: Some(1),
            sync_status: SyncStatus::Error,
            conflict_info: None,
        }
    }

    #[test]
    fn state_after_push_updates_existing_row_in_place() {
        let s = state_after_push(
            Some(baseline("u1", "old")),
            &push_action("u1"),
            Some("new".into()),
            (String::new(), String::new()),
            500,
        );
        assert_eq!(s.uuid, "u1");
        assert_eq!(s.local_hash.as_deref(), Some("new"));
        assert_eq!(s.remote_hash.as_deref(), Some("new"));
        assert_eq!(s.synced_hash.as_deref(), Some("new"));
        assert_eq!(s.synced_mtime, Some(500));
        assert_eq!(s.sync_status, SyncStatus::Synced);
    }

    #[test]
    fn state_after_push_creates_row_for_new_document() {
        let s = state_after_push(
            None,
            &push_action("u2"),
            Some("h".into()),
            ("parent".into(), "DocumentType".into()),
            7,
        );
        assert_eq!(s.parent_uuid, "parent");
        assert_eq!(s.synced_hash.as_deref(), Some("h"));
        assert_eq!(s.last_sync_at, Some(7));
    }

    #[test]
    fn malformed_metadata_and_content_are_rejected_before_upload() {
        let good_meta = br#"{"visibleName":"A","type":"DocumentType","parent":""}"#;
        assert!(validate_device_file("u.metadata", good_meta).is_ok());
        assert!(validate_device_file("u.metadata", b"{not json").is_err());
        assert!(validate_device_file("u.metadata", b"[]").is_err());
        assert!(validate_device_file("u.metadata", br#"{"type":"DocumentType"}"#).is_err());
        assert!(validate_device_file("u.metadata", br#"{"visibleName":1,"type":"x"}"#).is_err());
        assert!(validate_device_file("u.content", b"{}").is_ok());
        assert!(validate_device_file("u.content", b"").is_err());
        assert!(validate_device_file("u/p1.rm", b"\x00binary").is_ok(), "pages are not JSON");
    }

    #[test]
    fn metadata_is_uploaded_last() {
        let files = vec![
            (PathBuf::from("a"), "u.metadata".to_string()),
            (PathBuf::from("b"), "u.content".to_string()),
            (PathBuf::from("c"), "u/p1.rm".to_string()),
        ];
        let order: Vec<&str> = upload_order(&files).iter().map(|(_, r)| r.as_str()).collect();
        assert_eq!(order.last(), Some(&"u.metadata"));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn local_delete_keeps_a_backup_and_refuses_unsafe_uuids() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw");
        std::fs::create_dir_all(raw.join("abc")).unwrap();
        std::fs::write(raw.join("abc.metadata"), b"meta").unwrap();
        std::fs::write(raw.join("abc/p1.rm"), b"page").unwrap();
        let backup = backup_root(dir.path()).join("deleted-test/abc");

        backup_and_delete_local_document("abc", dir.path(), &backup).unwrap();

        assert!(!raw.join("abc.metadata").exists());
        assert!(!raw.join("abc").exists());
        assert_eq!(std::fs::read(backup.join("abc.metadata")).unwrap(), b"meta");
        assert_eq!(std::fs::read(backup.join("abc/p1.rm")).unwrap(), b"page");

        std::fs::write(raw.join("keep.metadata"), b"x").unwrap();
        assert!(backup_and_delete_local_document("../raw", dir.path(), &backup).is_err());
        assert!(raw.join("keep.metadata").exists());
    }
}
