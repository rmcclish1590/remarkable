//! Three-state diff and sync orchestration across local, remote, and DB state.
//!
//! `compute_sync_plan` is a pure function: given the current local manifest,
//! the current remote manifest, and the synced baseline in SQLite, it produces
//! an ordered list of `SyncAction`s. No files are touched; executors live in
//! specs 12/13.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::sync::scanner::{LocalDocumentSnapshot, LocalManifest, RemoteDocumentSnapshot, RemoteManifest};
use crate::sync::state_db::{StateDb, SyncFileState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncActionType {
    Pull,
    Push,
    DeleteLocal,
    DeleteRemote,
    Conflict { local_mtime: u64, remote_mtime: u64 },
    DeleteBoth,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncAction {
    pub uuid: String,
    pub visible_name: String,
    pub action_type: SyncActionType,
    pub priority: u8,
}

#[derive(Debug, Default)]
pub struct SyncPlan {
    pub actions: Vec<SyncAction>,
    pub total_pull: usize,
    pub total_push: usize,
    pub total_delete: usize,
    pub total_conflict: usize,
    pub total_skip: usize,
}

impl SyncPlan {
    pub fn has_conflicts(&self) -> bool {
        self.total_conflict > 0
    }

    pub fn conflicts(&self) -> Vec<&SyncAction> {
        self.actions
            .iter()
            .filter(|a| matches!(a.action_type, SyncActionType::Conflict { .. }))
            .collect()
    }

    pub fn actionable(&self) -> Vec<&SyncAction> {
        self.actions
            .iter()
            .filter(|a| !matches!(a.action_type, SyncActionType::Skip))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.actionable().is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "Pull {}, Push {}, Delete {}, Conflicts {}, Skip {}",
            self.total_pull, self.total_push, self.total_delete, self.total_conflict, self.total_skip
        )
    }
}

/// Compute the sync plan by diffing local, remote, and synced states.
pub fn compute_sync_plan(
    local: &LocalManifest,
    remote: &RemoteManifest,
    db: &StateDb,
) -> Result<SyncPlan> {
    let synced_vec = db.get_all_states()?;
    Ok(compute_sync_plan_from_parts(local, remote, &synced_vec))
}

/// Variant of `compute_sync_plan` that takes the synced-state vector directly.
/// Makes the diff trivially unit-testable without touching SQLite.
pub fn compute_sync_plan_from_parts(
    local: &LocalManifest,
    remote: &RemoteManifest,
    synced: &[SyncFileState],
) -> SyncPlan {
    let local_map: HashMap<&str, &LocalDocumentSnapshot> = local
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    let remote_map: HashMap<&str, &RemoteDocumentSnapshot> = remote
        .documents
        .iter()
        .map(|d| (d.uuid.as_str(), d))
        .collect();
    let synced_map: HashMap<&str, &SyncFileState> =
        synced.iter().map(|s| (s.uuid.as_str(), s)).collect();

    let mut all_uuids: HashSet<&str> = HashSet::new();
    all_uuids.extend(local_map.keys().copied());
    all_uuids.extend(remote_map.keys().copied());
    all_uuids.extend(synced_map.keys().copied());

    let mut actions = Vec::with_capacity(all_uuids.len());
    for uuid in all_uuids {
        let action = classify(
            uuid,
            local_map.get(uuid).copied(),
            remote_map.get(uuid).copied(),
            synced_map.get(uuid).copied(),
        );
        actions.push(action);
    }

    actions.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| action_rank(&a.action_type).cmp(&action_rank(&b.action_type)))
            .then_with(|| a.uuid.cmp(&b.uuid))
    });

    let mut plan = SyncPlan::default();
    for a in &actions {
        match &a.action_type {
            SyncActionType::Pull => plan.total_pull += 1,
            SyncActionType::Push => plan.total_push += 1,
            SyncActionType::DeleteLocal
            | SyncActionType::DeleteRemote
            | SyncActionType::DeleteBoth => plan.total_delete += 1,
            SyncActionType::Conflict { .. } => plan.total_conflict += 1,
            SyncActionType::Skip => plan.total_skip += 1,
        }
    }
    plan.actions = actions;
    plan
}

fn classify(
    uuid: &str,
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> SyncAction {
    let (visible_name, priority) = derive_name_and_priority(local, remote, synced);
    let action_type = match (local, remote, synced) {
        (Some(l), Some(r), Some(s)) => {
            let synced_hash = s.synced_hash.as_deref().unwrap_or("");
            let local_changed = l.content_hash != synced_hash;
            let remote_changed = r.content_hash != synced_hash;
            match (local_changed, remote_changed) {
                (false, false) => SyncActionType::Skip,
                (true, false) => SyncActionType::Push,
                (false, true) => SyncActionType::Pull,
                (true, true) => {
                    if l.content_hash == r.content_hash {
                        SyncActionType::Skip
                    } else {
                        SyncActionType::Conflict {
                            local_mtime: l.mtime,
                            remote_mtime: r.mtime,
                        }
                    }
                }
            }
        }
        (None, Some(_), None) => SyncActionType::Pull,
        (Some(_), None, None) => SyncActionType::Push,
        (Some(l), Some(r), None) => {
            if l.content_hash == r.content_hash {
                SyncActionType::Skip
            } else {
                SyncActionType::Conflict {
                    local_mtime: l.mtime,
                    remote_mtime: r.mtime,
                }
            }
        }
        (Some(l), None, Some(s)) => {
            if l.content_hash == s.synced_hash.as_deref().unwrap_or("") {
                SyncActionType::DeleteLocal
            } else {
                SyncActionType::Push
            }
        }
        (None, Some(r), Some(s)) => {
            if r.content_hash == s.synced_hash.as_deref().unwrap_or("") {
                SyncActionType::DeleteRemote
            } else {
                SyncActionType::Pull
            }
        }
        (None, None, Some(_)) => SyncActionType::DeleteBoth,
        (None, None, None) => SyncActionType::Skip,
    };

    SyncAction {
        uuid: uuid.to_string(),
        visible_name,
        action_type,
        priority,
    }
}

fn derive_name_and_priority(
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> (String, u8) {
    if let Some(r) = remote {
        let prio = if r.metadata.doc_type == "CollectionType" {
            0
        } else {
            1
        };
        return (r.metadata.visible_name.clone(), prio);
    }
    if let Some(l) = local {
        let prio = if l.metadata.doc_type == "CollectionType" {
            0
        } else {
            1
        };
        return (l.metadata.visible_name.clone(), prio);
    }
    if let Some(s) = synced {
        let prio = if s.doc_type == "CollectionType" { 0 } else { 1 };
        return (s.visible_name.clone(), prio);
    }
    (String::new(), 1)
}

fn action_rank(a: &SyncActionType) -> u8 {
    match a {
        SyncActionType::Pull => 0,
        SyncActionType::Push => 1,
        SyncActionType::Conflict { .. } => 2,
        SyncActionType::DeleteLocal => 3,
        SyncActionType::DeleteRemote => 4,
        SyncActionType::DeleteBoth => 5,
        SyncActionType::Skip => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remarkable::metadata::RemarkableMetadata;
    use crate::sync::state_db::SyncStatus;
    use std::path::PathBuf;

    fn meta(name: &str, doc_type: &str) -> RemarkableMetadata {
        serde_json::from_str(&format!(
            r#"{{"deleted":false,"lastModified":"1","parent":"","pinned":false,"type":"{doc_type}","visibleName":"{name}"}}"#
        ))
        .unwrap()
    }

    fn local(uuid: &str, hash: &str, doc_type: &str) -> LocalDocumentSnapshot {
        LocalDocumentSnapshot {
            uuid: uuid.into(),
            metadata: meta(uuid, doc_type),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 10,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn remote(uuid: &str, hash: &str, doc_type: &str) -> RemoteDocumentSnapshot {
        RemoteDocumentSnapshot {
            uuid: uuid.into(),
            metadata: meta(uuid, doc_type),
            content: None,
            content_hash: hash.into(),
            total_size_bytes: 0,
            mtime: 20,
            page_count: 0,
            has_pdf: false,
            file_list: vec![],
        }
    }
    fn synced(uuid: &str, hash: &str, doc_type: &str) -> SyncFileState {
        SyncFileState {
            uuid: uuid.into(),
            visible_name: uuid.into(),
            parent_uuid: String::new(),
            doc_type: doc_type.into(),
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

    fn plan_with(
        local_docs: Vec<LocalDocumentSnapshot>,
        remote_docs: Vec<RemoteDocumentSnapshot>,
        synced_states: Vec<SyncFileState>,
    ) -> SyncPlan {
        let local = LocalManifest {
            documents: local_docs,
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
            sync_dir: PathBuf::from("/tmp"),
        };
        let remote = RemoteManifest {
            documents: remote_docs,
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
        };
        compute_sync_plan_from_parts(&local, &remote, &synced_states)
    }

    fn action_for<'a>(plan: &'a SyncPlan, uuid: &str) -> &'a SyncAction {
        plan.actions.iter().find(|a| a.uuid == uuid).unwrap()
    }

    #[test]
    fn all_in_sync_is_skip() {
        let p = plan_with(
            vec![local("a", "h", "DocumentType")],
            vec![remote("a", "h", "DocumentType")],
            vec![synced("a", "h", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Skip));
        assert!(p.is_empty());
    }

    #[test]
    fn new_remote_document_is_pull() {
        let p = plan_with(vec![], vec![remote("a", "h", "DocumentType")], vec![]);
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
        assert_eq!(p.total_pull, 1);
    }

    #[test]
    fn new_local_document_is_push() {
        let p = plan_with(vec![local("a", "h", "DocumentType")], vec![], vec![]);
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
        assert_eq!(p.total_push, 1);
    }

    #[test]
    fn modified_remotely_is_pull() {
        let p = plan_with(
            vec![local("a", "h1", "DocumentType")],
            vec![remote("a", "h2", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
    }

    #[test]
    fn modified_locally_is_push() {
        let p = plan_with(
            vec![local("a", "h2", "DocumentType")],
            vec![remote("a", "h1", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
    }

    #[test]
    fn true_conflict_emits_conflict_with_mtimes() {
        let p = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        match &action_for(&p, "a").action_type {
            SyncActionType::Conflict { local_mtime, remote_mtime } => {
                assert_eq!(*local_mtime, 10);
                assert_eq!(*remote_mtime, 20);
            }
            _ => panic!("expected Conflict"),
        }
        assert_eq!(p.total_conflict, 1);
    }

    #[test]
    fn false_conflict_is_skip() {
        let p = plan_with(
            vec![local("a", "hSame", "DocumentType")],
            vec![remote("a", "hSame", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Skip));
    }

    #[test]
    fn deleted_remotely_unmodified_locally_is_delete_local() {
        let p = plan_with(
            vec![local("a", "h1", "DocumentType")],
            vec![],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteLocal
        ));
    }

    #[test]
    fn deleted_remotely_modified_locally_is_push() {
        let p = plan_with(
            vec![local("a", "h2", "DocumentType")],
            vec![],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Push));
    }

    #[test]
    fn deleted_locally_unmodified_remotely_is_delete_remote() {
        let p = plan_with(
            vec![],
            vec![remote("a", "h1", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteRemote
        ));
    }

    #[test]
    fn deleted_locally_modified_remotely_is_pull() {
        let p = plan_with(
            vec![],
            vec![remote("a", "h2", "DocumentType")],
            vec![synced("a", "h1", "DocumentType")],
        );
        assert!(matches!(action_for(&p, "a").action_type, SyncActionType::Pull));
    }

    #[test]
    fn deleted_both_sides_is_delete_both() {
        let p = plan_with(vec![], vec![], vec![synced("a", "h1", "DocumentType")]);
        assert!(matches!(
            action_for(&p, "a").action_type,
            SyncActionType::DeleteBoth
        ));
    }

    #[test]
    fn both_new_identical_is_skip_both_new_different_is_conflict() {
        let p_same = plan_with(
            vec![local("a", "h", "DocumentType")],
            vec![remote("a", "h", "DocumentType")],
            vec![],
        );
        assert!(matches!(action_for(&p_same, "a").action_type, SyncActionType::Skip));

        let p_diff = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType")],
            vec![],
        );
        assert!(matches!(
            action_for(&p_diff, "a").action_type,
            SyncActionType::Conflict { .. }
        ));
    }

    #[test]
    fn folders_sort_before_documents() {
        let p = plan_with(
            vec![],
            vec![
                remote("doc", "h", "DocumentType"),
                remote("fld", "h", "CollectionType"),
            ],
            vec![],
        );
        let first = &p.actions[0];
        assert_eq!(first.uuid, "fld");
        assert_eq!(first.priority, 0);
    }

    #[test]
    fn summary_counts_and_helpers() {
        let p = plan_with(
            vec![local("a", "hL", "DocumentType")],
            vec![remote("a", "hR", "DocumentType"), remote("b", "h", "DocumentType")],
            vec![synced("a", "h0", "DocumentType")],
        );
        assert_eq!(p.total_conflict, 1);
        assert_eq!(p.total_pull, 1);
        assert!(p.has_conflicts());
        assert_eq!(p.conflicts().len(), 1);
        assert!(!p.is_empty());
        let s = p.summary();
        assert!(s.contains("Pull 1"));
        assert!(s.contains("Conflicts 1"));
    }

    #[test]
    fn pure_none_combination_is_skip() {
        let p = plan_with(vec![], vec![], vec![]);
        assert_eq!(p.actions.len(), 0);
        assert!(p.is_empty());
    }

    #[test]
    fn compute_sync_plan_integrates_with_state_db() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_state(&synced("a", "h1", "DocumentType")).unwrap();
        let local = LocalManifest {
            documents: vec![local("a", "h1", "DocumentType")],
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
            sync_dir: PathBuf::from("/tmp"),
        };
        let rem = RemoteManifest {
            documents: vec![remote("a", "h2", "DocumentType")],
            scanned_at: 0,
            total_documents: 0,
            total_size_bytes: 0,
        };
        let plan = compute_sync_plan(&local, &rem, &db).unwrap();
        assert_eq!(plan.total_pull, 1);
    }
}
