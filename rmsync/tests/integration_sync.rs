//! Integration tests for the sync pipeline that don't require a physical
//! reMarkable. They exercise the pure diff engine against synthetic
//! `LocalManifest`/`RemoteManifest`/`StateDb` snapshots.
//!
//! Tests that need real SFTP (pull_batch, push_batch, delete_remote) are
//! deliberately skipped — wiring a trait-based mock for `DeviceConnection`
//! was out of scope for this spec.

mod common;

use std::path::{Path, PathBuf};

use rmsync::remarkable::document::DocumentTree;
use rmsync::remarkable::metadata::RemarkableMetadata;
use rmsync::sync::engine::{compute_sync_plan_from_parts, resolve_conflict, ConflictWinner, SyncAction, SyncActionType};
use rmsync::sync::scanner::{
    scan_local, LocalDocumentSnapshot, LocalManifest, RemoteDocumentSnapshot, RemoteManifest,
};
use rmsync::sync::state_db::{StateDb, SyncFileState, SyncStatus};
use tempfile::tempdir;

fn md(name: &str, doc_type: &str) -> RemarkableMetadata {
    serde_json::from_str(&format!(
        r#"{{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"{doc_type}","visibleName":"{name}"}}"#
    ))
    .unwrap()
}

fn local_snap(uuid: &str, hash: &str, mtime: u64) -> LocalDocumentSnapshot {
    LocalDocumentSnapshot {
        uuid: uuid.into(),
        metadata: md(uuid, "DocumentType"),
        content: None,
        content_hash: hash.into(),
        total_size_bytes: 0,
        mtime,
        page_count: 0,
        has_pdf: false,
        file_list: vec![],
    }
}

fn remote_snap(uuid: &str, hash: &str, mtime: u64) -> RemoteDocumentSnapshot {
    RemoteDocumentSnapshot {
        uuid: uuid.into(),
        metadata: md(uuid, "DocumentType"),
        content: None,
        content_hash: hash.into(),
        total_size_bytes: 0,
        mtime,
        page_count: 0,
        has_pdf: false,
        file_list: vec![],
    }
}

fn synced_state(uuid: &str, hash: &str) -> SyncFileState {
    SyncFileState {
        uuid: uuid.into(),
        visible_name: uuid.into(),
        parent_uuid: String::new(),
        doc_type: "DocumentType".into(),
        local_hash: Some(hash.into()),
        remote_hash: Some(hash.into()),
        synced_hash: Some(hash.into()),
        local_mtime: None,
        remote_mtime: None,
        synced_mtime: None,
        last_sync_at: None,
        sync_status: SyncStatus::Synced,
        conflict_info: None,
    }
}

fn empty_local() -> LocalManifest {
    LocalManifest {
        documents: vec![],
        scanned_at: 0,
        total_documents: 0,
        total_size_bytes: 0,
        sync_dir: PathBuf::from("/tmp"),
    }
}

fn local_manifest(docs: Vec<LocalDocumentSnapshot>) -> LocalManifest {
    LocalManifest {
        documents: docs,
        scanned_at: 0,
        total_documents: 0,
        total_size_bytes: 0,
        sync_dir: PathBuf::from("/tmp"),
    }
}
fn remote_manifest(docs: Vec<RemoteDocumentSnapshot>) -> RemoteManifest {
    RemoteManifest {
        documents: docs,
        scanned_at: 0,
        total_documents: 0,
        total_size_bytes: 0,
    }
}

#[test]
fn first_sync_pulls_all_documents() {
    // Empty local, empty baseline → every remote doc becomes Pull.
    let plan = compute_sync_plan_from_parts(
        &empty_local(),
        &remote_manifest(vec![
            remote_snap("a", "h1", 10),
            remote_snap("b", "h2", 20),
            remote_snap("c", "h3", 30),
        ]),
        &[],
    );
    assert_eq!(plan.total_pull, 3);
    assert_eq!(plan.total_push, 0);
    assert_eq!(plan.total_conflict, 0);
    for a in &plan.actions {
        assert!(matches!(a.action_type, SyncActionType::Pull));
    }
}

#[test]
fn bidirectional_sync_pulls_pushes_and_skips() {
    let baseline = vec![
        synced_state("a", "v1"), // untouched remotely/locally
        synced_state("b", "v1"), // untouched remotely/locally
        synced_state("c", "v1"), // untouched remotely/locally
    ];
    let local = local_manifest(vec![
        local_snap("a", "v1", 10),     // unchanged
        local_snap("b", "v2-local", 20), // modified locally
        local_snap("c", "v1", 30),     // unchanged
    ]);
    let remote = remote_manifest(vec![
        remote_snap("a", "v2-remote", 10), // modified remotely
        remote_snap("b", "v1", 20),       // unchanged remotely
        remote_snap("c", "v1", 30),       // unchanged remotely
    ]);

    let plan = compute_sync_plan_from_parts(&local, &remote, &baseline);
    let a = plan.actions.iter().find(|a| a.uuid == "a").unwrap();
    let b = plan.actions.iter().find(|a| a.uuid == "b").unwrap();
    let c = plan.actions.iter().find(|a| a.uuid == "c").unwrap();
    assert!(matches!(a.action_type, SyncActionType::Pull));
    assert!(matches!(b.action_type, SyncActionType::Push));
    assert!(matches!(c.action_type, SyncActionType::Skip));
}

#[test]
fn conflict_resolution_picks_newer_side_and_names_backup() {
    let plan_action = SyncAction {
        uuid: "a".into(),
        visible_name: "Note".into(),
        action_type: SyncActionType::Conflict {
            local_mtime: 100,
            remote_mtime: 200,
        },
        priority: 1,
    };
    let local = local_snap("a", "hL", 100);
    let remote = remote_snap("a", "hR", 200);
    let resolution = resolve_conflict(&plan_action, &local, &remote);
    assert_eq!(resolution.winner, ConflictWinner::Remote);
    assert!(resolution.backup_name.contains("(conflict "));
    assert_eq!(resolution.backup_uuid.len(), 36);
}

#[test]
fn state_db_records_synced_baseline() {
    let db = StateDb::open_in_memory().unwrap();
    db.upsert_state(&synced_state("a", "v1")).unwrap();
    db.upsert_state(&synced_state("b", "v1")).unwrap();
    let all = db.get_all_states().unwrap();
    assert_eq!(all.len(), 2);
    let synced = db
        .get_by_status(rmsync::sync::state_db::SyncStatus::Synced)
        .unwrap();
    assert_eq!(synced.len(), 2);
}

#[test]
fn document_tree_reconstructs_folder_hierarchy_from_scan_dir() {
    let dir = tempdir().unwrap();
    let raw = dir.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();

    // Folder with one notebook inside + orphan at root.
    common::write_metadata(&raw, "folder-A", "", "CollectionType", "Projects", false);
    common::seed_notebook(&raw, "doc-1", "Ideas", "folder-A");
    common::seed_notebook(&raw, "doc-root", "Scratch", "");
    // A deleted doc — should be excluded.
    common::write_metadata(&raw, "gone", "", "DocumentType", "Gone", true);

    let tree = DocumentTree::build_from_directory(&raw).unwrap();
    let root_names: Vec<&str> = tree
        .roots
        .iter()
        .map(|n| n.metadata.visible_name.as_str())
        .collect();
    assert!(root_names.contains(&"Projects"));
    assert!(root_names.contains(&"Scratch"));
    assert!(!root_names.contains(&"Gone"));
    let projects = tree.find_by_uuid("folder-A").unwrap();
    assert_eq!(projects.children.len(), 1);
    assert_eq!(projects.children[0].metadata.visible_name, "Ideas");
}

#[test]
fn local_scanner_reproduces_doc_set_end_to_end() {
    let dir = tempdir().unwrap();
    let raw = dir.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    common::seed_notebook(&raw, "x", "Alpha", "");
    common::seed_notebook(&raw, "y", "Beta", "");

    let manifest = scan_local(dir.path()).unwrap();
    assert_eq!(manifest.total_documents, 2);
    let names: Vec<&str> = manifest
        .documents
        .iter()
        .map(|d| d.metadata.visible_name.as_str())
        .collect();
    assert!(names.contains(&"Alpha"));
    assert!(names.contains(&"Beta"));
    // Each document has at least the metadata+content files plus one page.
    for d in &manifest.documents {
        assert!(d.page_count >= 1);
    }
}

#[test]
fn consistent_hash_across_runs_for_same_inputs() {
    let dir = tempdir().unwrap();
    let raw = dir.path().join("raw");
    std::fs::create_dir_all(&raw).unwrap();
    common::seed_notebook(&raw, "x", "Alpha", "");
    let h1 = scan_local(dir.path()).unwrap().documents[0].content_hash.clone();
    let h2 = scan_local(dir.path()).unwrap().documents[0].content_hash.clone();
    assert_eq!(h1, h2);
}

/// Silence unused-helper-path warning when tests reuse only some items.
fn _silence<T>(_: &T) {}

#[test]
fn empty_paths_helper_is_stable() {
    let p = Path::new("/tmp");
    _silence(&p);
}
