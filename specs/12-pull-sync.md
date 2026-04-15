# Spec 12 — Pull Sync (reMarkable → PC)

**Layer:** 3 — Sync Core  
**Dependencies:** 11 (diff engine), 06 (SSH/SFTP), 05 (SQLite state)  
**Estimated effort:** 1–2 hours  

## Objective

Implement the executor that downloads documents from the reMarkable to the local sync directory, based on Pull actions in the sync plan.

## Context

The diff engine (Spec 11) produces a `SyncPlan` containing `SyncAction` items. This spec implements the executor for `SyncActionType::Pull` — downloading all files belonging to a document UUID from the reMarkable over SFTP into `{sync_dir}/raw/`.

## Technical Requirements

### 1. Transfer module (`src/sync/transfer.rs`)

```rust
/// Result of a single pull operation.
#[derive(Debug)]
pub struct PullResult {
    pub uuid: String,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub success: bool,
    pub error: Option<String>,
}

/// Progress for the overall pull batch.
#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub current_file: String,
    pub current_uuid: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}
```

### 2. Pull implementation

```rust
/// Execute a single Pull action — download all files for one UUID.
pub async fn pull_document(
    conn: &DeviceConnection,
    uuid: &str,
    sync_dir: &Path,
) -> Result<PullResult>
```

Implementation:

1. Create `{sync_dir}/raw/{uuid}/` directory if it doesn't exist.
2. List all files on the remote belonging to this UUID:
   - `{XOCHITL_PATH}/{uuid}.metadata`
   - `{XOCHITL_PATH}/{uuid}.content`
   - `{XOCHITL_PATH}/{uuid}.pagedata` (if exists)
   - `{XOCHITL_PATH}/{uuid}.highlights` (if exists)
   - `{XOCHITL_PATH}/{uuid}.pdf` (if exists)
   - All files in `{XOCHITL_PATH}/{uuid}/` directory (page .rm files)
3. For each file, download to the corresponding local path under `{sync_dir}/raw/`.
4. Use **atomic writes**: download to `{path}.tmp`, then rename to `{path}`. This prevents partial files if the connection drops.
5. Preserve remote mtimes on local files using `std::fs::set_permissions` / `filetime` crate.
6. Return `PullResult` with transfer stats.

### 3. Batch pull

```rust
/// Execute all Pull actions from a sync plan.
pub async fn pull_batch<F>(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    sync_dir: &Path,
    db: &StateDb,
    progress_callback: F,
) -> Result<Vec<PullResult>>
where
    F: Fn(TransferProgress) + Send + 'static,
```

Implementation:

1. Filter `plan.actions` for `SyncActionType::Pull` items.
2. Calculate total bytes (sum of remote snapshots' `total_size_bytes`).
3. For each Pull action:
   a. Call `pull_document()`.
   b. On success: update SQLite state — set `local_hash = remote_hash`, `synced_hash = remote_hash`, `sync_status = synced`, `last_sync_at = now()`.
   c. On failure: set `sync_status = error`, log the error.
   d. Fire progress callback.
4. Return all results.

### 4. Delete-local execution

Also implement the `DeleteLocal` action handler in this spec:

```rust
/// Delete all local files for a document UUID.
pub fn delete_local_document(uuid: &str, sync_dir: &Path) -> Result<()>
```

1. Remove `{sync_dir}/raw/{uuid}/` directory and all contents.
2. Remove `{sync_dir}/raw/{uuid}.metadata`.
3. Remove `{sync_dir}/raw/{uuid}.content`.
4. Remove any other `{sync_dir}/raw/{uuid}.*` files.
5. Remove the record from SQLite state.

### 5. Error recovery

- If a pull fails mid-document, clean up any `.tmp` files.
- Log the failure but continue with remaining Pull actions (don't abort the batch).
- The failed document remains in `pending` state for retry on next sync.

## Files to Create/Modify

- `src/sync/transfer.rs` — pull implementation
- Add `filetime = "0.2"` to Cargo.toml for setting file timestamps.
- `src/sync/mod.rs` — export types

## Test Strategy

1. **Pull to empty directory** — mock SFTP responses for a single UUID (3 files), verify all files appear locally.
2. **Atomic write** — simulate a failure mid-download, verify no `.tmp` files are left behind.
3. **Batch pull** — queue 3 Pull actions, verify all execute and progress callback fires 3 times.
4. **Delete local** — create local files for a UUID, call `delete_local_document`, verify all are removed.
5. **Database update on success** — after pull, verify SQLite state shows `synced` status with correct hashes.
6. **Partial failure** — fail one pull in a batch of 3, verify the other 2 succeed and the failed one is marked `error`.

## Acceptance Criteria

1. `pull_document` downloads all files for a UUID atomically.
2. `pull_batch` processes all Pull actions with progress reporting.
3. SQLite state is updated after each successful pull.
4. Failed pulls don't abort the batch.
5. `delete_local_document` cleanly removes all local files for a UUID.
6. No `.tmp` files are left on failure.
7. All unit tests pass.
