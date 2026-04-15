//! SFTP push/pull transfer primitives used by the sync engine.
//!
//! Spec 12: pull_document, pull_batch, delete_local_document.
//! Spec 13 will add the corresponding push side.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use filetime::{set_file_mtime, FileTime};

use crate::device::connection::{DeviceConnection, RemoteFileInfo};
use crate::sync::engine::{SyncAction, SyncActionType, SyncPlan};
use crate::sync::scanner::{RAW_SUBDIR, XOCHITL_PATH};
use crate::sync::state_db::StateDb;

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
        let local_path = local_path_for(&raw, uuid, &f.path);
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

fn local_path_for(raw: &Path, uuid: &str, remote_path: &str) -> PathBuf {
    let subdir_prefix = format!("{XOCHITL_PATH}/{uuid}/");
    if let Some(rel) = remote_path.strip_prefix(&subdir_prefix) {
        return raw.join(uuid).join(rel);
    }
    let top_prefix = format!("{XOCHITL_PATH}/");
    let rel = remote_path.strip_prefix(&top_prefix).unwrap_or(remote_path);
    raw.join(rel)
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
            if let Some(mut state) = db.get_state(&result.uuid)? {
                state.local_hash = state.remote_hash.clone();
                state.synced_hash = state.remote_hash.clone();
                state.synced_mtime = state.remote_mtime;
                state.sync_status = crate::sync::state_db::SyncStatus::Synced;
                state.last_sync_at = Some(now_secs());
                db.upsert_state(&state)?;
            }
        } else if let Some(mut state) = db.get_state(&result.uuid)? {
            state.sync_status = crate::sync::state_db::SyncStatus::Error;
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn local_path_maps_top_level_file() {
        let raw = Path::new("/sync/raw");
        let out = local_path_for(raw, "abc", &format!("{XOCHITL_PATH}/abc.metadata"));
        assert_eq!(out, PathBuf::from("/sync/raw/abc.metadata"));
    }

    #[test]
    fn local_path_maps_subdir_page() {
        let raw = Path::new("/sync/raw");
        let out = local_path_for(raw, "abc", &format!("{XOCHITL_PATH}/abc/p1.rm"));
        assert_eq!(out, PathBuf::from("/sync/raw/abc/p1.rm"));
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

    #[tokio::test]
    async fn cleanup_tmp_removes_existing_tmp() {
        let dir = tempdir().unwrap();
        let local = dir.path().join("x.rm");
        let tmp = tmp_path(&local);
        tokio::fs::write(&tmp, b"partial").await.unwrap();
        cleanup_tmp(&local).await;
        assert!(!tmp.exists());
    }
}
