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

/// Validate a uuid-like identifier: hex digits and `-` only, length-bounded.
/// Rejects anything that could escape a path component or pollute globs.
fn is_safe_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid.len() <= 64
        && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Validate that a single path component (name between `/`s) is safe to use
/// as a filesystem path component. Rejects `.`, `..`, empty, `/` embedded,
/// NUL bytes, and leading dots (so adversarial dotfiles can't appear).
fn is_safe_component(name: &str) -> bool {
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
                None => SyncFileState {
                    uuid: action.uuid.clone(),
                    visible_name: action.visible_name.clone(),
                    parent_uuid: String::new(),
                    doc_type: "DocumentType".into(),
                    local_hash: local_hash.clone(),
                    remote_hash: local_hash.clone(),
                    synced_hash: local_hash,
                    local_mtime: Some(now),
                    remote_mtime: Some(now),
                    synced_mtime: Some(now),
                    last_sync_at: Some(now),
                    sync_status: SyncStatus::Synced,
                    conflict_info: None,
                },
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
    let subdir_remote = format!("{XOCHITL_PATH}/{uuid}");
    if local_files.iter().any(|(_, rel)| rel.starts_with(&format!("{uuid}/"))) {
        let _ = conn.mkdir(&subdir_remote).await;
    }

    let mut transferred = 0usize;
    let mut bytes = 0u64;
    let mut first_err: Option<String> = None;
    for (local_path, rel) in &local_files {
        let remote_path = format!("{XOCHITL_PATH}/{rel}");
        match upload_atomic(conn, local_path, &remote_path).await {
            Ok(n) => {
                transferred += 1;
                bytes += n;
            }
            Err(e) => {
                first_err.get_or_insert_with(|| format!("{}: {e}", remote_path));
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

async fn upload_atomic(
    conn: &DeviceConnection,
    local_path: &Path,
    remote_path: &str,
) -> Result<u64> {
    let tmp_remote = format!("{remote_path}.tmp");
    let data = tokio::fs::read(local_path).await?;
    conn.write_file(&tmp_remote, &data).await?;
    // russh-sftp has no atomic-rename helper on SftpSession; use its underlying
    // raw client by calling rename via the session (available as sftp.rename).
    conn.sftp()?
        .rename(&tmp_remote, remote_path)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(data.len() as u64)
}

fn local_files_for_uuid(raw: &Path, uuid: &str) -> Result<Vec<(PathBuf, String)>> {
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
        let result = push_document(conn, &action.uuid, sync_dir).await?;
        if result.success {
            let local_hash = scan_local_hash_for_uuid(
                &action.uuid,
                &sync_dir.join(crate::sync::scanner::RAW_SUBDIR),
            );
            let now = now_secs();
            let state = match db.get_state(&result.uuid)? {
                Some(mut existing) => {
                    existing.remote_hash = local_hash.clone();
                    existing.synced_hash = local_hash;
                    existing.sync_status = SyncStatus::Synced;
                    existing.last_sync_at = Some(now);
                    existing
                }
                None => SyncFileState {
                    uuid: action.uuid.clone(),
                    visible_name: action.visible_name.clone(),
                    parent_uuid: String::new(),
                    doc_type: "DocumentType".into(),
                    local_hash: local_hash.clone(),
                    remote_hash: local_hash.clone(),
                    synced_hash: local_hash,
                    local_mtime: Some(now),
                    remote_mtime: Some(now),
                    synced_mtime: Some(now),
                    last_sync_at: Some(now),
                    sync_status: SyncStatus::Synced,
                    conflict_info: None,
                },
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
        bytes_total: 0,
    });
    Ok(results)
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
    Ok(())
}

/// Ask the reMarkable to reload its document list after a batch of
/// mutations. Restarting xochitl causes a brief screen refresh — invoke this
/// once at the end of a sync, not per-file.
pub async fn reload_xochitl(conn: &mut DeviceConnection) -> Result<()> {
    let status = conn.exec("systemctl restart xochitl").await?;
    if status != 0 {
        tracing::warn!("systemctl restart xochitl exited with {status}");
    }
    Ok(())
}

/// Compute a local content hash for a single UUID after pull/push. Falls
/// back to an empty string if the files can't be read — the next sync will
/// detect the mismatch and re-classify.
fn scan_local_hash_for_uuid(uuid: &str, raw: &Path) -> Option<String> {
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

/// Execute a resolved conflict. On Remote-wins we preserve the local copy
/// under a new backup UUID then pull the remote. On Local-wins we pull the
/// remote under the backup UUID, keep local files in place, and push the
/// original UUID back to the device.
pub async fn execute_conflict_resolution(
    conn: &DeviceConnection,
    resolution: &ConflictResolution,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
    sync_dir: &Path,
    db: &StateDb,
) -> Result<()> {
    let raw = sync_dir.join(RAW_SUBDIR);
    tokio::fs::create_dir_all(&raw).await?;
    match resolution.winner {
        ConflictWinner::Remote => {
            copy_local_as_backup(&raw, &resolution.uuid, &resolution.backup_uuid, &resolution.backup_name)?;
            let _ = pull_document(conn, &resolution.uuid, sync_dir).await?;
            record_winner_state(db, &resolution.uuid, local, remote, ConflictWinner::Remote)?;
            record_backup_state(db, &resolution.backup_uuid, &resolution.backup_name, local)?;
        }
        ConflictWinner::Local => {
            pull_document_to_backup(conn, &resolution.uuid, &resolution.backup_uuid, &resolution.backup_name, sync_dir).await?;
            let _ = push_document(conn, &resolution.uuid, sync_dir).await?;
            record_winner_state(db, &resolution.uuid, local, remote, ConflictWinner::Local)?;
            record_backup_state(db, &resolution.backup_uuid, &resolution.backup_name, local)?;
        }
    }
    Ok(())
}

fn copy_local_as_backup(
    raw: &Path,
    uuid: &str,
    backup_uuid: &str,
    backup_name: &str,
) -> Result<()> {
    for entry in std::fs::read_dir(raw)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(suffix) = name.strip_prefix(&format!("{uuid}.")) {
            let dest = raw.join(format!("{backup_uuid}.{suffix}"));
            if suffix == "metadata" {
                let mut md: RemarkableMetadata = serde_json::from_slice(&std::fs::read(entry.path())?)?;
                md.visible_name = backup_name.to_string();
                std::fs::write(&dest, serde_json::to_vec_pretty(&md)?)?;
            } else {
                std::fs::copy(entry.path(), &dest)?;
            }
        }
    }
    let subdir = raw.join(uuid);
    if subdir.is_dir() {
        let dest_dir = raw.join(backup_uuid);
        std::fs::create_dir_all(&dest_dir)?;
        for entry in std::fs::read_dir(&subdir)? {
            let entry = entry?;
            std::fs::copy(entry.path(), dest_dir.join(entry.file_name()))?;
        }
    }
    Ok(())
}

async fn pull_document_to_backup(
    conn: &DeviceConnection,
    uuid: &str,
    backup_uuid: &str,
    backup_name: &str,
    sync_dir: &Path,
) -> Result<()> {
    let raw = sync_dir.join(RAW_SUBDIR);
    tokio::fs::create_dir_all(&raw).await?;
    if !is_safe_uuid(uuid) || !is_safe_uuid(backup_uuid) {
        return Err(anyhow::anyhow!(
            "refusing to pull backup for unsafe uuid {uuid} / {backup_uuid}"
        ));
    }
    let remote_files = list_remote_files_for_uuid(conn, uuid).await?;
    for f in &remote_files {
        // Resolve the remote path safely under the ORIGINAL uuid first, then
        // rebase onto backup_uuid. Skip anything a malicious tablet tries to
        // smuggle outside raw/.
        let safe = match safe_local_path_for(&raw, uuid, &f.path) {
            Some(p) => p,
            None => {
                tracing::warn!("rejecting unsafe backup remote path: {}", f.path);
                continue;
            }
        };
        // `safe` is guaranteed to live under `raw` (and under `raw/{uuid}`
        // or `raw/` directly). Rebase by replacing the uuid component(s).
        let rel = safe.strip_prefix(&raw).expect("safe_local_path_for guarantees prefix");
        let mut remapped = raw.clone();
        for component in rel.components() {
            let s = component.as_os_str().to_string_lossy();
            if s == uuid {
                remapped.push(backup_uuid);
            } else if s.starts_with(&format!("{uuid}.")) {
                remapped.push(format!("{backup_uuid}.{}", &s[uuid.len() + 1..]));
            } else {
                remapped.push(component);
            }
        }
        let local_path = remapped;
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = conn.read_file(&f.path).await?;
        let tmp = tmp_path(&local_path);
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &local_path).await?;
    }
    let backup_meta = raw.join(format!("{backup_uuid}.metadata"));
    if backup_meta.exists() {
        let bytes = std::fs::read(&backup_meta)?;
        if let Ok(mut md) = serde_json::from_slice::<RemarkableMetadata>(&bytes) {
            md.visible_name = backup_name.to_string();
            std::fs::write(&backup_meta, serde_json::to_vec_pretty(&md)?)?;
        }
    }
    Ok(())
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

fn record_backup_state(
    db: &StateDb,
    backup_uuid: &str,
    backup_name: &str,
    local: &LocalDocumentSnapshot,
) -> Result<()> {
    let state = SyncFileState {
        uuid: backup_uuid.to_string(),
        visible_name: backup_name.to_string(),
        parent_uuid: local.metadata.parent.clone(),
        doc_type: local.metadata.doc_type.clone(),
        local_hash: Some(local.content_hash.clone()),
        remote_hash: None,
        synced_hash: None,
        local_mtime: Some(local.mtime),
        remote_mtime: None,
        synced_mtime: None,
        last_sync_at: None,
        sync_status: SyncStatus::Pending,
        conflict_info: None,
    };
    db.upsert_state(&state)?;
    Ok(())
}

/// Resolve every Conflict action in a plan using last-write-wins.
pub async fn resolve_all_conflicts(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    local_manifest: &LocalManifest,
    remote_manifest: &RemoteManifest,
    sync_dir: &Path,
    db: &StateDb,
) -> Result<Vec<ConflictResolution>> {
    let mut out = Vec::new();
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
        if let (Some(l), Some(r)) = (
            local_map.get(action.uuid.as_str()),
            remote_map.get(action.uuid.as_str()),
        ) {
            let resolution = crate::sync::engine::resolve_conflict(action, l, r);
            execute_conflict_resolution(conn, &resolution, l, r, sync_dir, db).await?;
            out.push(resolution);
        }
    }
    Ok(out)
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
    fn copy_local_as_backup_preserves_user_work_and_renames_metadata() {
        // Conflict-resolution backup path: when remote wins we MUST preserve
        // the user's local copy byte-for-byte (page files) and rewrite only
        // the visibleName in metadata. A bug here silently destroys work.
        let dir = tempdir().unwrap();
        let raw = dir.path();
        let meta = r#"{"deleted":false,"lastModified":"1","parent":"p","pinned":false,"type":"DocumentType","visibleName":"Original"}"#;
        std::fs::write(raw.join("abc.metadata"), meta).unwrap();
        std::fs::write(raw.join("abc.content"), b"content-bytes").unwrap();
        std::fs::create_dir_all(raw.join("abc")).unwrap();
        std::fs::write(raw.join("abc/p1.rm"), b"page-1-bytes").unwrap();
        std::fs::write(raw.join("other.metadata"), b"untouched").unwrap();

        copy_local_as_backup(raw, "abc", "backup-uuid", "Original (conflict)").unwrap();

        // Original files must be untouched — this is the user's preserved copy.
        assert_eq!(std::fs::read(raw.join("abc.content")).unwrap(), b"content-bytes");
        assert_eq!(std::fs::read(raw.join("abc/p1.rm")).unwrap(), b"page-1-bytes");
        assert_eq!(std::fs::read(raw.join("other.metadata")).unwrap(), b"untouched");

        // Page files must be copied byte-for-byte under the backup uuid.
        assert_eq!(
            std::fs::read(raw.join("backup-uuid/p1.rm")).unwrap(),
            b"page-1-bytes"
        );
        assert_eq!(
            std::fs::read(raw.join("backup-uuid.content")).unwrap(),
            b"content-bytes"
        );

        // Metadata must carry the new visibleName and still be valid JSON.
        let backup_meta: RemarkableMetadata =
            serde_json::from_slice(&std::fs::read(raw.join("backup-uuid.metadata")).unwrap())
                .unwrap();
        assert_eq!(backup_meta.visible_name, "Original (conflict)");
        assert_eq!(backup_meta.parent, "p");
    }
}
