# Spec 13 — Push Sync (PC → reMarkable)

**Layer:** 3 — Sync Core  
**Dependencies:** 11 (diff engine), 06 (SSH/SFTP), 05 (SQLite state)  
**Estimated effort:** 1–2 hours  

## Objective

Implement the executor that uploads documents from the local sync directory to the reMarkable, based on Push actions in the sync plan.

## Context

When a document is modified locally (or created locally), the diff engine classifies it as a Push action. This spec uploads all files for that UUID to the reMarkable over SFTP and restarts the xochitl process so the device picks up the changes.

## Technical Requirements

### 1. Push implementation (`src/sync/transfer.rs` — extend)

```rust
/// Result of a single push operation.
#[derive(Debug)]
pub struct PushResult {
    pub uuid: String,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub success: bool,
    pub error: Option<String>,
}

/// Upload all local files for a document UUID to the reMarkable.
pub async fn push_document(
    conn: &DeviceConnection,
    uuid: &str,
    sync_dir: &Path,
) -> Result<PushResult>
```

Implementation:

1. Verify local files exist at `{sync_dir}/raw/{uuid}.metadata` (minimum requirement).
2. Collect all local files for this UUID:
   - `{sync_dir}/raw/{uuid}.metadata`
   - `{sync_dir}/raw/{uuid}.content`
   - `{sync_dir}/raw/{uuid}.pagedata` (if exists)
   - `{sync_dir}/raw/{uuid}.highlights` (if exists)
   - `{sync_dir}/raw/{uuid}.pdf` (if exists)
   - All files in `{sync_dir}/raw/{uuid}/` directory
3. Ensure the remote directory `{XOCHITL_PATH}/{uuid}/` exists (create via `conn.mkdir()` if needed).
4. Upload each file:
   - Upload to `{remote_path}.tmp` first.
   - Rename to `{remote_path}` (atomic write on device).
5. Return `PushResult` with stats.

### 2. Batch push

```rust
/// Execute all Push actions from a sync plan.
pub async fn push_batch<F>(
    conn: &DeviceConnection,
    plan: &SyncPlan,
    sync_dir: &Path,
    db: &StateDb,
    progress_callback: F,
) -> Result<Vec<PushResult>>
where
    F: Fn(TransferProgress) + Send + 'static,
```

Same pattern as `pull_batch`:
1. Filter for Push actions.
2. For each: upload, update SQLite on success, log on failure.
3. Fire progress callback.

### 3. Delete-remote execution

```rust
/// Delete all remote files for a document UUID on the reMarkable.
pub async fn delete_remote_document(
    conn: &DeviceConnection,
    uuid: &str,
) -> Result<()>
```

1. Delete `{XOCHITL_PATH}/{uuid}/` directory and all contents.
2. Delete `{XOCHITL_PATH}/{uuid}.metadata`.
3. Delete `{XOCHITL_PATH}/{uuid}.content`.
4. Delete any other `{XOCHITL_PATH}/{uuid}.*` files.
5. Remove the record from SQLite state.

### 4. Xochitl restart

After a batch of pushes (or deletes), the reMarkable's UI process (`xochitl`) needs to be notified to pick up changes. The simplest approach:

```rust
/// Signal the reMarkable to reload its document list.
/// Sends SIGUSR1 to xochitl, or restarts the service.
pub async fn reload_xochitl(conn: &DeviceConnection) -> Result<()>
```

Implementation options (try in order):
1. `systemctl restart xochitl` — full restart (reliable but causes brief UI flash on device).
2. Alternative: create a `.reload` marker file that some xochitl versions watch.

> **Note:** This causes a brief screen refresh on the reMarkable. It's expected behavior. Only trigger this once after all pushes/deletes complete, not per-file.

### 5. Error recovery

- Same as pull: atomic writes, clean up `.tmp` on failure, continue batch on individual failure.
- If xochitl restart fails, log a warning but don't fail the sync — files are already on the device.

## Files to Create/Modify

- `src/sync/transfer.rs` — add push implementation
- `src/sync/mod.rs` — export new types

## Test Strategy

1. **Push single document** — create local files for a UUID, mock SFTP, verify upload calls.
2. **Atomic upload** — verify `.tmp` suffix is used during upload.
3. **Batch push** — queue 3 Push actions, verify all execute with progress.
4. **Delete remote** — verify correct SFTP delete calls for all file types.
5. **Database update on success** — verify SQLite shows `synced` with matching hashes.
6. **Missing local files** — attempt push for a UUID with no local `.metadata`, verify error.
7. **Xochitl reload** — verify SSH command is sent after batch completes.

## Acceptance Criteria

1. `push_document` uploads all files for a UUID atomically.
2. `push_batch` processes all Push actions with progress reporting.
3. SQLite state is updated after each successful push.
4. `delete_remote_document` removes all remote files for a UUID.
5. `reload_xochitl` is called once after all pushes/deletes complete.
6. Failed pushes don't abort the batch.
7. All unit tests pass.
