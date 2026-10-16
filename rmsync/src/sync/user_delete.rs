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
use crate::sync::transfer::{backup_root, copy_local_to_backup, delete_local_document, is_safe_uuid};

const INFO_FILE: &str = "info.json";

/// Delete `uuid` locally and queue its removal from the device.
///
/// Returns the backup directory holding a copy of everything removed. If the
/// local removal fails part-way, the files are restored from that backup and
/// the queued delete is withdrawn, so the document is never left half-deleted.
pub fn request_delete(sync_dir: &Path, db: &StateDb, uuid: &str, name: &str, now: u64) -> Result<PathBuf> {
    request_delete_with(sync_dir, db, uuid, name, now, remove_local_files)
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

    let backup_dir = backup_root(sync_dir)
        .join("user-deleted")
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
}
