//! Cleanup of "(conflict YYYY-MM-DD)" duplicates left by earlier syncs.

mod common;

use std::path::Path;

use filetime::{set_file_mtime, FileTime};
use rmsync::sync::cleanup::{apply_cleanup, plan_cleanup, CleanupOutcome};
use rmsync::sync::state_db::StateDb;
use tempfile::tempdir;

const NOW: u64 = 1_790_000_000;

fn raw_of(dir: &Path) -> std::path::PathBuf {
    let raw = dir.join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    raw
}

/// An original plus a duplicate named like the old sync made it.
fn seed_pair(raw: &Path) {
    common::seed_notebook(raw, "0a1b", "Meeting", "");
    common::seed_notebook(raw, "0d1e", "Meeting (conflict 2026-10-02)", "");
}

fn stroke(raw: &Path, uuid: &str) {
    common::create_rm_v6_with_stroke(&raw.join(uuid).join("page1.rm"), &[(0.0, 0.0), (5.0, 5.0)]).unwrap();
}

/// Make `uuid` look last modified at `secs`: both the metadata (what the
/// cleanup trusts) and the page file. `name` must be the document's name.
fn set_modified(raw: &Path, uuid: &str, name: &str, secs: u64) {
    common::write_metadata_modified(raw, uuid, "", "DocumentType", name, false, secs * 1000);
    set_file_mtime(raw.join(uuid).join("page1.rm"), FileTime::from_unix_time(secs as i64, 0)).unwrap();
}

fn only_backup_run(dir: &Path) -> std::path::PathBuf {
    let root = dir.join(".rmsync").join("backups");
    let runs: Vec<_> = std::fs::read_dir(root).unwrap().flatten().collect();
    assert_eq!(runs.len(), 1);
    runs[0].path()
}

#[test]
fn identical_duplicate_is_removed_and_recoverable() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);

    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].outcome, CleanupOutcome::RemoveIdentical);
    assert_eq!(findings[0].original_uuid, "0a1b");

    let db = StateDb::open_in_memory().unwrap();
    let report = apply_cleanup(dir.path(), &db, &findings, NOW);
    assert_eq!(report.removed, 1);
    assert!(report.errors.is_empty());

    assert!(!raw.join("0d1e.metadata").exists());
    assert!(!raw.join("0d1e").exists());
    assert!(raw.join("0a1b.metadata").exists());
    // Moved, not deleted.
    let run = only_backup_run(dir.path());
    assert!(run.join("0d1e/0d1e.metadata").exists());
    assert!(run.join("0d1e/0d1e/page1.rm").exists());
}

#[test]
fn newer_original_is_kept_and_differing_duplicate_is_backed_up() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    stroke(&raw, "0d1e");
    set_modified(&raw, "0d1e", "Meeting (conflict 2026-10-02)", 1_000);
    set_modified(&raw, "0a1b", "Meeting", 2_000);
    let original_bytes = std::fs::read(raw.join("0a1b/page1.rm")).unwrap();
    let dup_bytes = std::fs::read(raw.join("0d1e/page1.rm")).unwrap();

    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(findings[0].outcome, CleanupOutcome::KeepOriginal);
    apply_cleanup(dir.path(), &StateDb::open_in_memory().unwrap(), &findings, NOW);

    assert_eq!(std::fs::read(raw.join("0a1b/page1.rm")).unwrap(), original_bytes);
    let run = only_backup_run(dir.path());
    assert_eq!(std::fs::read(run.join("0d1e/0d1e/page1.rm")).unwrap(), dup_bytes);
}

#[test]
fn newer_duplicate_replaces_original_content_and_old_original_is_backed_up() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    stroke(&raw, "0d1e");
    set_modified(&raw, "0a1b", "Meeting", 1_000);
    set_modified(&raw, "0d1e", "Meeting (conflict 2026-10-02)", 2_000);
    let original_bytes = std::fs::read(raw.join("0a1b/page1.rm")).unwrap();
    let dup_bytes = std::fs::read(raw.join("0d1e/page1.rm")).unwrap();

    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(findings[0].outcome, CleanupOutcome::KeepDuplicate);
    let report = apply_cleanup(dir.path(), &StateDb::open_in_memory().unwrap(), &findings, NOW);
    assert_eq!((report.removed, report.originals_replaced), (1, 1));

    // Original now carries the newer content but keeps its own name.
    assert_eq!(std::fs::read(raw.join("0a1b/page1.rm")).unwrap(), dup_bytes);
    let meta = std::fs::read_to_string(raw.join("0a1b.metadata")).unwrap();
    assert!(meta.contains("\"Meeting\""), "{meta}");
    // The version that was overwritten is recoverable.
    let run = only_backup_run(dir.path());
    assert_eq!(std::fs::read(run.join("0a1b/0a1b/page1.rm")).unwrap(), original_bytes);
    assert!(!raw.join("0d1e.metadata").exists());
}

#[test]
fn library_without_duplicates_is_left_untouched() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    common::seed_notebook(&raw, "a", "Alpha", "");
    common::seed_notebook(&raw, "b", "Beta", "");
    assert!(plan_cleanup(dir.path()).unwrap().is_empty());
    assert!(plan_cleanup(&dir.path().join("missing")).unwrap().is_empty());
    assert!(!dir.path().join(".rmsync").exists());
}

#[test]
fn orphan_duplicate_with_no_original_is_not_touched() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    common::seed_notebook(&raw, "0d1e", "Lost (conflict 2026-10-02)", "");
    assert!(plan_cleanup(dir.path()).unwrap().is_empty());
    assert!(raw.join("0d1e.metadata").exists());
}

#[test]
fn original_in_a_different_folder_is_not_a_match() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    common::write_metadata(&raw, "folder", "", "CollectionType", "Work", false);
    common::seed_notebook(&raw, "0a1b", "Meeting", "folder");
    common::seed_notebook(&raw, "0d1e", "Meeting (conflict 2026-10-02)", "");
    assert!(plan_cleanup(dir.path()).unwrap().is_empty());
}

#[test]
fn rerunning_cleanup_is_a_no_op() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    let db = StateDb::open_in_memory().unwrap();

    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(apply_cleanup(dir.path(), &db, &findings, NOW).removed, 1);

    // Same stale confirmation applied again, and a fresh plan: both do nothing.
    let again = apply_cleanup(dir.path(), &db, &findings, NOW + 5);
    assert_eq!((again.removed, again.skipped), (0, 1));
    assert!(plan_cleanup(dir.path()).unwrap().is_empty());
    assert_eq!(std::fs::read_dir(dir.path().join(".rmsync/backups")).unwrap().count(), 1);
}

#[test]
fn stale_confirmation_cannot_overwrite_an_original_it_was_not_confirmed_to() {
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    // Confirmed as identical...
    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(findings[0].outcome, CleanupOutcome::RemoveIdentical);
    // ...then the duplicate is edited and becomes newer before apply.
    stroke(&raw, "0d1e");
    set_modified(&raw, "0a1b", "Meeting", 1_000);
    set_modified(&raw, "0d1e", "Meeting (conflict 2026-10-02)", 2_000);
    let original_bytes = std::fs::read(raw.join("0a1b/page1.rm")).unwrap();

    let report = apply_cleanup(dir.path(), &StateDb::open_in_memory().unwrap(), &findings, NOW);
    assert_eq!(report.skipped, 1);
    assert_eq!(std::fs::read(raw.join("0a1b/page1.rm")).unwrap(), original_bytes);
    assert!(raw.join("0d1e.metadata").exists());
}

#[test]
fn removed_duplicate_baseline_lets_the_next_sync_delete_the_device_copy() {
    use rmsync::sync::engine::{compute_sync_plan_from_parts, SyncActionType};
    use rmsync::sync::scanner::{scan_local, LocalManifest, RemoteDocumentSnapshot, RemoteManifest};
    use rmsync::sync::state_db::{SyncFileState, SyncStatus};

    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    let before = scan_local(dir.path()).unwrap();
    let dup_hash = before.documents.iter().find(|d| d.uuid == "0d1e").unwrap().content_hash.clone();

    // The earlier sync recorded the duplicate as pending, with no synced hash.
    let db = StateDb::open_in_memory().unwrap();
    db.upsert_state(&SyncFileState {
        uuid: "0d1e".into(),
        visible_name: "Meeting (conflict 2026-10-02)".into(),
        parent_uuid: String::new(),
        doc_type: "DocumentType".into(),
        local_hash: Some(dup_hash.clone()),
        remote_hash: None,
        synced_hash: None,
        local_mtime: None,
        remote_mtime: None,
        synced_mtime: None,
        last_sync_at: None,
        sync_status: SyncStatus::Pending,
        conflict_info: None,
    })
    .unwrap();

    let findings = plan_cleanup(dir.path()).unwrap();
    apply_cleanup(dir.path(), &db, &findings, NOW);

    // The device still holds exactly what we moved away.
    let local: LocalManifest = scan_local(dir.path()).unwrap();
    let dup_local = before.documents.iter().find(|d| d.uuid == "0d1e").unwrap();
    let remote = RemoteManifest {
        documents: vec![RemoteDocumentSnapshot {
            uuid: "0d1e".into(),
            metadata: dup_local.metadata.clone(),
            content: None,
            content_hash: dup_hash,
            total_size_bytes: 0,
            mtime: 1,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }],
        scanned_at: 0,
        total_documents: 1,
        total_size_bytes: 0,
    };
    let plan = compute_sync_plan_from_parts(&local, &remote, &db.get_all_states().unwrap());
    let a = plan.actions.iter().find(|a| a.uuid == "0d1e").unwrap();
    assert_eq!(a.action_type, SyncActionType::DeleteRemote);
}

#[test]
fn a_freshly_copied_stale_backup_does_not_look_newer_than_the_original() {
    // The old sync copied the losing desktop version, stamping its files with
    // the time of the conflict. Its metadata still says when it was really
    // edited, which is what decides.
    let dir = tempdir().unwrap();
    let raw = raw_of(dir.path());
    seed_pair(&raw);
    stroke(&raw, "0d1e");
    set_modified(&raw, "0a1b", "Meeting", 2_000);
    set_modified(&raw, "0d1e", "Meeting (conflict 2026-10-02)", 1_000);
    // Page file of the duplicate is much newer than anything else on disk.
    set_file_mtime(raw.join("0d1e/page1.rm"), FileTime::from_unix_time(9_000_000, 0)).unwrap();

    let findings = plan_cleanup(dir.path()).unwrap();
    assert_eq!(findings[0].outcome, CleanupOutcome::KeepOriginal);
}
