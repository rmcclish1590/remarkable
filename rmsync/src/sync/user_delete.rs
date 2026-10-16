//! Deleting a document on purpose from the desktop app.
//!
//! The flow is: copy the document's files to a backup, record the intent in
//! the state DB, remove the local files and rendered pages, and let the next
//! sync remove the device copy. The recorded intent (`pending_deletes`) is
//! what makes that safe: it overrides the three-state diff, so the document is
//! never re-pulled from the device, and it survives an unreachable tablet
//! until the device copy is really gone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sync::engine::{SyncAction, SyncActionType, SyncPlan};
use crate::sync::scanner::{RemoteManifest, RAW_SUBDIR};
use crate::sync::state_db::{PendingDelete, StateDb};
use crate::sync::transfer::{
    backup_root, copy_local_to_backup, delete_local_document, is_safe_uuid, local_files_for_uuid,
    verify_file_set,
};

const INFO_FILE: &str = "info.json";
/// Backups of documents the user deleted: `backups/user-deleted/<unix-ts>_<uuid>/`.
const USER_DELETED_DIR: &str = "user-deleted";
const SECS_PER_DAY: u64 = 86_400;

/// Delete `uuid` locally and queue its removal from the device.
///
/// Returns the backup directory holding a copy of everything removed. If the
/// local removal fails part-way, the files are restored from that backup and
/// the queued delete is withdrawn, so the document is never left half-deleted.
pub fn request_delete(sync_dir: &Path, db: &StateDb, uuid: &str, name: &str, now: u64) -> Result<PathBuf> {
    // Names and ids only; page content is never logged.
    let result = request_delete_with(sync_dir, db, uuid, name, now, remove_local_files);
    match &result {
        Ok(backup) => tracing::info!(
            uuid, name, timestamp = now, backup = %backup.display(), result = "ok",
            "user deleted document"
        ),
        Err(e) => tracing::error!(
            uuid, name, timestamp = now, error = format!("{e:#}"), result = "failed",
            "user delete failed"
        ),
    }
    result
}

/// `request_delete` with the local removal injectable, so the failure path can be tested.
pub(crate) fn request_delete_with(
    sync_dir: &Path,
    db: &StateDb,
    uuid: &str,
    name: &str,
    now: u64,
    remove: impl Fn(&str, &Path) -> Result<()>,
) -> Result<PathBuf> {
    anyhow::ensure!(is_safe_uuid(uuid), "refusing to delete unsafe uuid {uuid:?}");
    let raw = sync_dir.join(RAW_SUBDIR);
    anyhow::ensure!(
        raw.join(format!("{uuid}.metadata")).is_file(),
        "“{name}” ({uuid}) is not in the local library"
    );

    // Everything about to be removed must belong to this one document.
    let files = local_files_for_uuid(&raw, uuid)?;
    verify_file_set(uuid, files.iter().map(|(_, rel)| rel.as_str()))?;

    let backup_dir = backup_root(sync_dir)
        .join(USER_DELETED_DIR)
        .join(format!("{now}_{uuid}"));
    copy_local_to_backup(&raw, uuid, &backup_dir).context("backing up before delete")?;
    let info = serde_json::json!({ "uuid": uuid, "name": name, "deleted_at": now });
    std::fs::write(backup_dir.join(INFO_FILE), serde_json::to_vec_pretty(&info)?)
        .context("writing backup info")?;

    // Intent first: if the app dies after this point the delete is still owed
    // and the next sync finishes it rather than pulling the document back.
    db.add_pending_delete(uuid, name)?;

    if let Err(e) = remove(uuid, sync_dir) {
        let restored = restore_backup(&backup_dir, &raw);
        let _ = db.clear_pending_delete(uuid);
        return Err(match restored {
            Ok(()) => e.context(format!("could not delete “{name}”; it was left as it was")),
            Err(r) => e.context(format!(
                "could not delete “{name}” and restoring it failed too ({r:#}); a copy is in {}",
                backup_dir.display()
            )),
        });
    }
    Ok(backup_dir)
}

/// Remove `uuid`'s files from `raw/` and its rendered pages from the cache.
fn remove_local_files(uuid: &str, sync_dir: &Path) -> Result<()> {
    delete_local_document(uuid, sync_dir)?;
    let cache = sync_dir.join(".rmsync").join("cache");
    if let Ok(entries) = std::fs::read_dir(&cache) {
        let prefix = format!("{uuid}_");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("removing {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

/// Copy every file in `backup_dir` (except the info file) back under `raw/`,
/// keeping the layout it was backed up with.
pub(crate) fn restore_backup(backup_dir: &Path, raw: &Path) -> Result<()> {
    fn walk(src: &Path, dst: &Path, top: bool) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            if top && name == INFO_FILE {
                continue;
            }
            let to = dst.join(&name);
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), &to, false)?;
            } else {
                std::fs::copy(entry.path(), &to)
                    .with_context(|| format!("restoring {}", to.display()))?;
            }
        }
        Ok(())
    }
    walk(backup_dir, raw, true)
}

/// A backup of a document the user deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedBackup {
    pub dir: PathBuf,
    pub uuid: String,
    pub name: String,
    pub deleted_at: u64,
}

/// Backups of user-deleted documents, newest first.
pub fn list_deleted_backups(sync_dir: &Path) -> Vec<DeletedBackup> {
    let root = backup_root(sync_dir).join(USER_DELETED_DIR);
    let mut out: Vec<DeletedBackup> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let info: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.path().join(INFO_FILE)).ok()?).ok()?;
            Some(DeletedBackup {
                dir: entry.path(),
                uuid: info.get("uuid")?.as_str()?.to_string(),
                name: info.get("name")?.as_str()?.to_string(),
                deleted_at: info.get("deleted_at")?.as_u64()?,
            })
        })
        .collect();
    out.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    out
}

/// Put a deleted document back under `raw/`. It is then a local-only document,
/// so the next sync uploads it to the tablet again. Refuses to overwrite a
/// document that exists, and withdraws any still-queued device delete for it.
pub fn restore_deleted(sync_dir: &Path, db: &StateDb, backup: &DeletedBackup) -> Result<()> {
    anyhow::ensure!(is_safe_uuid(&backup.uuid), "backup has an unsafe uuid {:?}", backup.uuid);
    let raw = sync_dir.join(RAW_SUBDIR);
    anyhow::ensure!(
        !raw.join(format!("{}.metadata", backup.uuid)).exists(),
        "“{}” already exists in the library",
        backup.name
    );
    restore_backup(&backup.dir, &raw)?;
    db.clear_pending_delete(&backup.uuid)?;
    tracing::info!(uuid = %backup.uuid, name = %backup.name, result = "ok", "restored deleted document from backup");
    Ok(())
}

/// Remove backups of deleted documents older than `retention_days`: user
/// deletes and sync-driven deletes. Conflict and cleanup backups are left
/// alone. `retention_days == 0` keeps everything. Returns how many were removed.
pub fn prune_deleted_backups(sync_dir: &Path, now: u64, retention_days: u32) -> Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let cutoff = now.saturating_sub(retention_days as u64 * SECS_PER_DAY);
    let root = backup_root(sync_dir);
    let mut candidates: Vec<(PathBuf, Option<u64>)> = Vec::new();
    for entry in std::fs::read_dir(root.join(USER_DELETED_DIR)).into_iter().flatten().flatten() {
        let ts = entry.file_name().to_string_lossy().split('_').next().and_then(|t| t.parse().ok());
        candidates.push((entry.path(), ts));
    }
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("deleted-") {
            // deleted-<date>-<unix-ts>
            candidates.push((entry.path(), name.rsplit('-').next().and_then(|t| t.parse().ok())));
        }
    }
    let mut removed = 0;
    for (dir, ts) in candidates {
        // A backup whose age cannot be read is kept: better clutter than loss.
        let Some(ts) = ts else { continue };
        if ts < cutoff && dir.is_dir() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("pruning {}", dir.display()))?;
            tracing::info!(backup = %dir.display(), age_days = (now - ts) / SECS_PER_DAY, "pruned old deleted-document backup");
            removed += 1;
        }
    }
    Ok(removed)
}

/// Make the plan carry out the user's queued deletes.
///
/// Whatever the diff concluded for a queued document (a pull, because the
/// device copy changed after the last sync; a skip; nothing at all), the user's
/// decision wins: remove it from the device if it is there, otherwise there is
/// nothing left to do on either side.
pub fn apply_pending_deletes(plan: &mut SyncPlan, pending: &[PendingDelete], remote: &RemoteManifest) {
    for p in pending {
        let on_device = remote.documents.iter().any(|d| d.uuid == p.uuid);
        let action_type = if on_device { SyncActionType::DeleteRemote } else { SyncActionType::DeleteBoth };
        match plan.actions.iter_mut().find(|a| a.uuid == p.uuid) {
            Some(a) => a.action_type = action_type,
            None => plan.actions.push(SyncAction {
                uuid: p.uuid.clone(),
                visible_name: p.name.clone(),
                action_type,
                priority: 1,
            }),
        }
    }
    plan.finalize();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn library(dir: &Path, uuids: &[&str]) {
        let raw = dir.join("raw");
        std::fs::create_dir_all(&raw).unwrap();
        for u in uuids {
            std::fs::write(raw.join(format!("{u}.metadata")), format!(r#"{{"visibleName":"{u}","type":"DocumentType"}}"#)).unwrap();
            std::fs::write(raw.join(format!("{u}.content")), b"{}").unwrap();
            std::fs::create_dir_all(raw.join(u)).unwrap();
            std::fs::write(raw.join(u).join("p1.rm"), b"page").unwrap();
        }
        let cache = dir.join(".rmsync/cache");
        std::fs::create_dir_all(&cache).unwrap();
        for u in uuids {
            std::fs::write(cache.join(format!("{u}_p1.svg")), b"<svg/>").unwrap();
        }
    }

    #[test]
    fn deletes_only_the_target_and_queues_the_device_removal() {
        let dir = tempdir().unwrap();
        library(dir.path(), &["aaa", "aaa-copy"]);
        let db = StateDb::open_in_memory().unwrap();

        let backup = request_delete(dir.path(), &db, "aaa", "Notes", 100).unwrap();

        let raw = dir.path().join("raw");
        assert!(!raw.join("aaa.metadata").exists());
        assert!(!raw.join("aaa").exists());
        assert!(!dir.path().join(".rmsync/cache/aaa_p1.svg").exists());
        // A similarly named document is untouched.
        assert!(raw.join("aaa-copy.metadata").exists());
        assert!(raw.join("aaa-copy/p1.rm").exists());
        assert!(dir.path().join(".rmsync/cache/aaa-copy_p1.svg").exists());
        // Backed up, and the intent is recorded.
        assert!(backup.join("aaa.metadata").exists());
        assert!(backup.join("aaa/p1.rm").exists());
        let pending = db.pending_deletes().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].uuid.as_str(), pending[0].name.as_str()), ("aaa", "Notes"));
    }

    #[test]
    fn local_failure_restores_everything_and_withdraws_the_request() {
        let dir = tempdir().unwrap();
        library(dir.path(), &["aaa"]);
        let db = StateDb::open_in_memory().unwrap();

        // Fails after removing the metadata: a half-deleted document.
        let err = request_delete_with(dir.path(), &db, "aaa", "Notes", 100, |u, d| {
            std::fs::remove_file(d.join("raw").join(format!("{u}.metadata")))?;
            anyhow::bail!("disk on fire")
        })
        .unwrap_err();

        assert!(format!("{err:#}").contains("left as it was"));
        let raw = dir.path().join("raw");
        assert!(raw.join("aaa.metadata").exists(), "restored");
        assert!(raw.join("aaa.content").exists());
        assert!(raw.join("aaa/p1.rm").exists());
        assert!(db.pending_deletes().unwrap().is_empty(), "no delete left queued");
    }

    #[test]
    fn unknown_or_unsafe_ids_are_refused_before_anything_happens() {
        let dir = tempdir().unwrap();
        library(dir.path(), &["aaa"]);
        let db = StateDb::open_in_memory().unwrap();
        assert!(request_delete(dir.path(), &db, "../raw", "x", 1).is_err());
        assert!(request_delete(dir.path(), &db, "bbb", "x", 1).is_err());
        assert!(db.pending_deletes().unwrap().is_empty());
        assert!(dir.path().join("raw/aaa.metadata").exists());
    }

    fn remote_with(uuids: &[&str]) -> RemoteManifest {
        use crate::remarkable::metadata::RemarkableMetadata;
        use crate::sync::scanner::RemoteDocumentSnapshot;
        let meta: RemarkableMetadata =
            serde_json::from_str(r#"{"visibleName":"n","type":"DocumentType","parent":"","lastModified":"1"}"#).unwrap();
        RemoteManifest {
            documents: uuids
                .iter()
                .map(|u| RemoteDocumentSnapshot {
                    uuid: u.to_string(),
                    metadata: meta.clone(),
                    content: None,
                    content_hash: "changed-on-device".into(),
                    total_size_bytes: 0,
                    mtime: 1,
                    page_count: 0,
                    has_pdf: false,
                    file_list: vec![],
                })
                .collect(),
            scanned_at: 0,
            total_documents: uuids.len(),
            total_size_bytes: 0,
        }
    }

    fn pending(uuid: &str) -> PendingDelete {
        PendingDelete { uuid: uuid.into(), name: format!("doc {uuid}"), requested_at: 1 }
    }

    #[test]
    fn a_queued_delete_overrides_a_pull_so_the_document_is_not_resurrected() {
        let mut plan = SyncPlan::default();
        plan.actions.push(SyncAction {
            uuid: "aaa".into(),
            visible_name: "Notes".into(),
            action_type: SyncActionType::Pull,
            priority: 1,
        });
        plan.actions.push(SyncAction {
            uuid: "other".into(),
            visible_name: "Other".into(),
            action_type: SyncActionType::Pull,
            priority: 1,
        });
        plan.finalize();

        apply_pending_deletes(&mut plan, &[pending("aaa")], &remote_with(&["aaa", "other"]));

        let by = |u: &str| plan.actions.iter().find(|a| a.uuid == u).unwrap().action_type.clone();
        assert_eq!(by("aaa"), SyncActionType::DeleteRemote);
        assert_eq!(by("other"), SyncActionType::Pull, "other documents are untouched");
        assert_eq!((plan.total_pull, plan.total_delete), (1, 1));
    }

    #[test]
    fn a_queued_delete_with_no_diff_entry_is_added_and_one_already_gone_is_just_cleared() {
        let mut plan = SyncPlan::default();
        apply_pending_deletes(&mut plan, &[pending("aaa"), pending("bbb")], &remote_with(&["aaa"]));
        let by = |u: &str| plan.actions.iter().find(|a| a.uuid == u).unwrap().action_type.clone();
        assert_eq!(by("aaa"), SyncActionType::DeleteRemote);
        assert_eq!(by("bbb"), SyncActionType::DeleteBoth, "already off the device");
        assert_eq!(plan.total_delete, 2);
    }

    #[test]
    fn pending_deletes_persist_and_clear() {
        let db = StateDb::open_in_memory().unwrap();
        db.add_pending_delete("aaa", "Notes").unwrap();
        db.add_pending_delete("aaa", "Notes").unwrap();
        assert_eq!(db.pending_deletes().unwrap().len(), 1);
        db.clear_pending_delete("aaa").unwrap();
        assert!(db.pending_deletes().unwrap().is_empty());
    }

    #[test]
    fn backup_is_created_before_delete_and_can_be_restored() {
        let dir = tempdir().unwrap();
        library(dir.path(), &["aaa"]);
        let db = StateDb::open_in_memory().unwrap();
        request_delete(dir.path(), &db, "aaa", "My Notes", 500).unwrap();

        let backups = list_deleted_backups(dir.path());
        assert_eq!(backups.len(), 1);
        assert_eq!((backups[0].uuid.as_str(), backups[0].name.as_str(), backups[0].deleted_at), ("aaa", "My Notes", 500));
        assert!(!dir.path().join("raw/aaa.metadata").exists());

        restore_deleted(dir.path(), &db, &backups[0]).unwrap();

        let raw = dir.path().join("raw");
        assert_eq!(std::fs::read(raw.join("aaa/p1.rm")).unwrap(), b"page");
        assert!(raw.join("aaa.metadata").exists() && raw.join("aaa.content").exists());
        assert!(!raw.join(INFO_FILE).exists(), "the info file is not a document file");
        assert!(db.pending_deletes().unwrap().is_empty(), "queued device delete withdrawn");
        // Restoring again would overwrite; refused.
        assert!(restore_deleted(dir.path(), &db, &backups[0]).is_err());
    }

    #[test]
    fn retention_prunes_old_deleted_backups_only() {
        let dir = tempdir().unwrap();
        let root = dir.path().join(".rmsync/backups");
        let day = SECS_PER_DAY;
        let now = 100 * day;
        for (rel, keep) in [
            (format!("user-deleted/{}_old", now - 40 * day), false),
            (format!("user-deleted/{}_new", now - 5 * day), true),
            (format!("deleted-2026-01-01-{}", now - 31 * day), false),
            (format!("deleted-2026-01-01-{}", now - day), true),
            ("cleanup-2026-10-15-1".to_string(), true),
            ("user-deleted/unparseable".to_string(), true),
            ("some-conflict-uuid".to_string(), true),
        ] {
            std::fs::create_dir_all(root.join(&rel)).unwrap();
            std::fs::write(root.join(&rel).join("f"), b"x").unwrap();
            let _ = keep;
        }
        assert_eq!(prune_deleted_backups(dir.path(), now, 30).unwrap(), 2);
        assert!(!root.join(format!("user-deleted/{}_old", now - 40 * day)).exists());
        assert!(root.join(format!("user-deleted/{}_new", now - 5 * day)).exists());
        assert!(!root.join(format!("deleted-2026-01-01-{}", now - 31 * day)).exists());
        assert!(root.join(format!("deleted-2026-01-01-{}", now - day)).exists());
        assert!(root.join("cleanup-2026-10-15-1").exists());
        assert!(root.join("user-deleted/unparseable").exists());
        assert!(root.join("some-conflict-uuid").exists());
        // 0 keeps everything.
        assert_eq!(prune_deleted_backups(dir.path(), now * 10, 0).unwrap(), 0);
    }
}
