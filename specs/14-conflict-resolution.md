# Spec 14 — Conflict Detection & Resolution

**Layer:** 3 — Sync Core  
**Dependencies:** 11 (diff engine), 12 (pull sync), 13 (push sync)  
**Estimated effort:** 1–2 hours  

## Objective

Implement the conflict resolution strategy that handles documents modified on both the PC and the reMarkable since the last sync, using last-write-wins with automatic backup of the losing version.

## Context

When the diff engine detects both the local and remote hashes differ from the synced baseline (and differ from each other), it's a true conflict. We need a resolution strategy that doesn't lose data. The approach: the newer version (by mtime) wins and becomes the synced version, while the older version is preserved as a named backup.

## Technical Requirements

### 1. Conflict resolver (`src/sync/engine.rs` — extend)

```rust
#[derive(Debug, Clone)]
pub struct ConflictResolution {
    pub uuid: String,
    pub visible_name: String,
    pub winner: ConflictWinner,
    pub backup_uuid: String,          // UUID assigned to the backup copy
    pub backup_name: String,          // e.g., "Meeting Notes (conflict 2026-04-12)"
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictWinner {
    Local,    // Local version wins, remote becomes backup
    Remote,   // Remote version wins, local becomes backup
}

/// Resolve a conflict action using last-write-wins strategy.
pub fn resolve_conflict(
    action: &SyncAction,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
) -> ConflictResolution
```

Implementation:

1. Compare `local.mtime` and `remote.mtime`.
2. The newer mtime wins. If equal, remote wins (prefer device as source of truth for handwriting).
3. Generate a backup name: `"{visible_name} (conflict {YYYY-MM-DD})"`.
4. Generate a new UUID for the backup copy.
5. Return the `ConflictResolution`.

### 2. Execute conflict resolution

```rust
/// Execute a conflict resolution.
pub async fn execute_conflict_resolution(
    conn: &DeviceConnection,
    resolution: &ConflictResolution,
    local: &LocalDocumentSnapshot,
    remote: &RemoteDocumentSnapshot,
    sync_dir: &Path,
    db: &StateDb,
) -> Result<()>
```

Implementation for `ConflictWinner::Remote` (remote wins):

1. **Backup the local version:**
   a. Copy all local files for the UUID to new files with `backup_uuid`:
      - `{uuid}.metadata` → `{backup_uuid}.metadata` (update `visibleName` to backup_name)
      - `{uuid}.content` → `{backup_uuid}.content`
      - `{uuid}/` → `{backup_uuid}/` (copy all .rm files with new page UUIDs or keep as-is)
   b. The backup lives locally only — it's a new local-only document.

2. **Pull the remote version** as the winner:
   - Call `pull_document(conn, uuid, sync_dir)`.

3. **Update SQLite:**
   - Mark the original UUID as `synced` with the remote hash.
   - Create a new sync state entry for `backup_uuid` as a local-only document (will be pushed on next sync, or the user can delete it).

Implementation for `ConflictWinner::Local` (local wins):

1. **Backup the remote version:**
   a. Download remote files to `{sync_dir}/raw/{backup_uuid}.*` and `{sync_dir}/raw/{backup_uuid}/`.
   b. Create a `.metadata` file for the backup with the backup_name.

2. **Push the local version** to the device:
   - Call `push_document(conn, uuid, sync_dir)`.

3. **Update SQLite:**
   - Mark the original UUID as `synced` with the local hash.
   - Create a new entry for the backup UUID.

### 3. Batch conflict resolution

```rust
/// Resolve all conflicts in a sync plan using LWW strategy.
/// Returns the resolutions for UI display.
pub async fn resolve_all_conflicts(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    local_manifest: &LocalManifest,
    remote_manifest: &RemoteManifest,
    sync_dir: &Path,
    db: &StateDb,
) -> Result<Vec<ConflictResolution>>
```

### 4. Conflict notification struct (for UI)

```rust
/// Information about a resolved conflict, for display in the UI.
#[derive(Debug, Clone)]
pub struct ConflictNotification {
    pub document_name: String,
    pub winner_source: String,         // "local" or "reMarkable"
    pub winner_mtime: u64,
    pub loser_mtime: u64,
    pub backup_name: String,
    pub time_difference_human: String, // e.g., "local was 2 hours newer"
}

impl ConflictResolution {
    pub fn to_notification(&self, local_mtime: u64, remote_mtime: u64) -> ConflictNotification
}
```

## Files to Create/Modify

- `src/sync/engine.rs` — add conflict resolution types and logic
- `src/sync/transfer.rs` — add `execute_conflict_resolution`
- `src/sync/mod.rs` — export new types

## Test Strategy

1. **Remote wins (newer)** — local mtime = 100, remote mtime = 200 → `ConflictWinner::Remote`.
2. **Local wins (newer)** — local mtime = 200, remote mtime = 100 → `ConflictWinner::Local`.
3. **Equal mtime** — both same → `ConflictWinner::Remote` (device preference).
4. **Backup naming** — verify backup name follows `"{name} (conflict {date})"` pattern.
5. **Backup UUID generation** — verify a new valid UUID is generated.
6. **Execute remote-wins** — verify local backup is created, remote is pulled, SQLite updated.
7. **Execute local-wins** — verify remote backup is downloaded, local is pushed, SQLite updated.
8. **Multiple conflicts** — resolve 3 conflicts, verify all produce correct results.

## Acceptance Criteria

1. Last-write-wins correctly identifies the winner by mtime.
2. The losing version is preserved as a named backup — no data loss.
3. Backup documents are properly structured (valid .metadata with descriptive name).
4. SQLite state is updated for both the winner and the backup.
5. Conflict notifications contain all information needed for UI display.
6. All unit tests pass.
