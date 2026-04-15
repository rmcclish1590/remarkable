# Spec 11 — Three-State Diff Engine

**Layer:** 3 — Sync Core  
**Dependencies:** 05 (SQLite state), 08 (remote scanner), 09 (local scanner)  
**Estimated effort:** 2–3 hours  

## Objective

Implement the core diff algorithm that compares local state, remote state, and the last-synced baseline to produce a deterministic sync plan — the ordered list of actions needed to bring both sides into alignment.

## Context

This is the brain of the sync engine. Inspired by Dropbox's three-tree architecture, it compares three views of every document: what's on the PC now, what's on the reMarkable now, and what both sides looked like after the last successful sync. By comparing all three, it can unambiguously determine change direction without ambiguity.

## Technical Requirements

### 1. Sync plan types (`src/sync/engine.rs`)

```rust
/// A single action to be executed during sync.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncAction {
    pub uuid: String,
    pub visible_name: String,
    pub action_type: SyncActionType,
    pub priority: u8,              // 0 = highest (folders first), 1 = documents
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncActionType {
    /// Download from reMarkable to local.
    Pull,
    /// Upload from local to reMarkable.
    Push,
    /// Delete the local copy (deleted on remote since last sync).
    DeleteLocal,
    /// Delete the remote copy (deleted locally since last sync).
    DeleteRemote,
    /// Both sides changed — requires conflict resolution.
    Conflict {
        local_mtime: u64,
        remote_mtime: u64,
    },
    /// Both sides deleted — clean up sync state.
    DeleteBoth,
    /// No action needed — already in sync.
    Skip,
}

/// The complete plan for a sync operation.
#[derive(Debug)]
pub struct SyncPlan {
    pub actions: Vec<SyncAction>,
    pub total_pull: usize,
    pub total_push: usize,
    pub total_delete: usize,
    pub total_conflict: usize,
    pub total_skip: usize,
}
```

### 2. Diff algorithm

```rust
/// Compute the sync plan by diffing local, remote, and synced states.
pub fn compute_sync_plan(
    local: &LocalManifest,
    remote: &RemoteManifest,
    db: &StateDb,
) -> Result<SyncPlan>
```

Implementation logic:

1. **Build lookup maps:**
   - `local_map: HashMap<String, &LocalDocumentSnapshot>` — keyed by UUID
   - `remote_map: HashMap<String, &RemoteDocumentSnapshot>` — keyed by UUID
   - `synced_states: HashMap<String, SyncFileState>` — all records from SQLite

2. **Collect all known UUIDs** from the union of all three maps.

3. **For each UUID, classify the action:**

```
let local = local_map.get(uuid);
let remote = remote_map.get(uuid);
let synced = synced_states.get(uuid);

match (local, remote, synced) {
    // Both exist, baseline exists — compare hashes
    (Some(l), Some(r), Some(s)) => {
        let local_changed = l.content_hash != s.synced_hash.as_deref().unwrap_or("");
        let remote_changed = r.content_hash != s.synced_hash.as_deref().unwrap_or("");
        
        match (local_changed, remote_changed) {
            (false, false) => Skip,
            (true, false)  => Push,
            (false, true)  => Pull,
            (true, true)   => {
                if l.content_hash == r.content_hash {
                    Skip  // Both changed identically — false conflict
                } else {
                    Conflict { local_mtime: l.mtime, remote_mtime: r.mtime }
                }
            }
        }
    }
    
    // New on remote only (not in synced state, not local)
    (None, Some(_), None) => Pull,
    
    // New on local only (not in synced state, not remote)
    (Some(_), None, None) => Push,
    
    // Both new — not in synced state but present on both sides
    (Some(l), Some(r), None) => {
        if l.content_hash == r.content_hash {
            Skip  // Identical — just record in synced state
        } else {
            Conflict { local_mtime: l.mtime, remote_mtime: r.mtime }
        }
    }
    
    // Deleted from remote (was synced, now only local)
    (Some(l), None, Some(s)) => {
        if l.content_hash == s.synced_hash.as_deref().unwrap_or("") {
            DeleteLocal  // Not modified locally — safe to delete
        } else {
            Push  // Modified locally after remote deletion — push to restore
        }
    }
    
    // Deleted from local (was synced, now only remote)
    (None, Some(r), Some(s)) => {
        if r.content_hash == s.synced_hash.as_deref().unwrap_or("") {
            DeleteRemote  // Not modified remotely — safe to delete
        } else {
            Pull  // Modified remotely after local deletion — pull to restore
        }
    }
    
    // Deleted from both sides
    (None, None, Some(_)) => DeleteBoth,
    
    // Shouldn't happen — UUID exists nowhere
    (None, None, None) => Skip,
}
```

4. **Sort actions:**
   - Folders (CollectionType) before documents (priority 0 vs 1).
   - Within each priority: Pull before Push before Delete.
   - This ensures parent folders exist before their documents are transferred.

5. **Compute summary counts** and build `SyncPlan`.

### 3. Plan validation

```rust
impl SyncPlan {
    /// Check if the plan has any conflicts that need resolution.
    pub fn has_conflicts(&self) -> bool
    
    /// Get only the conflict actions.
    pub fn conflicts(&self) -> Vec<&SyncAction>
    
    /// Get only actionable items (excluding Skip).
    pub fn actionable(&self) -> Vec<&SyncAction>
    
    /// Check if the plan is empty (nothing to do).
    pub fn is_empty(&self) -> bool
    
    /// Human-readable summary string.
    pub fn summary(&self) -> String
    // e.g., "Pull 3, Push 2, Delete 1, Conflicts 1, Skip 40"
}
```

### 4. Dry-run support

The sync plan is a data structure, not an executor. This enables:
- Displaying the plan to the user before executing.
- Logging the plan for debugging.
- Modifying the plan (e.g., user resolves conflicts before execution).

## Files to Create/Modify

- `src/sync/engine.rs` — full implementation
- `src/sync/mod.rs` — export types

## Test Strategy

This is the most critical component to test. Every edge case matters.

1. **All in sync** — local, remote, and synced all have same hashes → all Skip.
2. **New remote document** — exists on remote, not local, not synced → Pull.
3. **New local document** — exists locally, not remote, not synced → Push.
4. **Modified remotely** — remote hash differs from synced, local matches synced → Pull.
5. **Modified locally** — local hash differs from synced, remote matches synced → Push.
6. **True conflict** — both hashes differ from synced, and differ from each other → Conflict.
7. **False conflict** — both hashes differ from synced, but are identical to each other → Skip.
8. **Deleted remotely, unmodified locally** — remote gone, local matches synced → DeleteLocal.
9. **Deleted remotely, modified locally** — remote gone, local differs from synced → Push.
10. **Deleted locally, unmodified remotely** — local gone, remote matches synced → DeleteRemote.
11. **Deleted locally, modified remotely** — local gone, remote differs from synced → Pull.
12. **Deleted both sides** → DeleteBoth.
13. **First sync (no synced state)** — all documents appear as new. Remote-only → Pull, Local-only → Push.
14. **Folder ordering** — verify folders come before documents in the plan.
15. **Summary** — verify `SyncPlan::summary()` produces correct counts.

## Acceptance Criteria

1. `compute_sync_plan` correctly classifies every combination of local/remote/synced state.
2. False conflicts (identical changes on both sides) are detected and skipped.
3. Folders are ordered before their child documents.
4. The plan is a pure data structure — no side effects, no file operations.
5. All 15+ unit tests pass.
6. Edge cases around missing/None synced hashes are handled gracefully.
