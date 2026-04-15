# Spec 05 — SQLite Sync State Database

**Layer:** 0 — Foundation  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 1 hour  

## Objective

Implement the SQLite database layer that tracks sync state for every file, enabling the three-state diff algorithm to determine what changed locally, remotely, or on both sides since the last sync.

## Context

Bi-directional sync requires knowing the state of every file at three points: the current local state, the current remote state, and the state at the last successful sync (the "synced" baseline). This database is that baseline. It is stored at `{sync_destination}/.rmsync/state.db`.

## Technical Requirements

### 1. Database schema (`src/sync/state_db.rs`)

```sql
-- Applied on first run via embedded migration
CREATE TABLE IF NOT EXISTS sync_state (
    uuid            TEXT PRIMARY KEY,
    visible_name    TEXT NOT NULL,
    parent_uuid     TEXT NOT NULL DEFAULT '',
    doc_type        TEXT NOT NULL,           -- 'DocumentType' or 'CollectionType'
    local_hash      TEXT,                    -- SHA-256 of local file bundle
    remote_hash     TEXT,                    -- SHA-256 of remote file bundle
    synced_hash     TEXT,                    -- hash at last successful sync
    local_mtime     INTEGER,                -- local modification timestamp (ms)
    remote_mtime    INTEGER,                -- remote modification timestamp (ms)
    synced_mtime    INTEGER,                -- mtime at last successful sync
    last_sync_at    INTEGER,                -- unix timestamp of last sync completion
    sync_status     TEXT NOT NULL DEFAULT 'pending',  -- pending|synced|conflict|error
    conflict_info   TEXT,                   -- JSON blob with conflict details
    created_at      INTEGER NOT NULL,       -- when this row was first created
    updated_at      INTEGER NOT NULL        -- when this row was last modified
);

CREATE INDEX IF NOT EXISTS idx_sync_status ON sync_state(sync_status);
CREATE INDEX IF NOT EXISTS idx_parent ON sync_state(parent_uuid);

CREATE TABLE IF NOT EXISTS sync_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid            TEXT NOT NULL,
    action          TEXT NOT NULL,           -- push|pull|delete_local|delete_remote|conflict|skip
    direction       TEXT,                    -- 'to_local' or 'to_remote'
    timestamp       INTEGER NOT NULL,
    hash_before     TEXT,
    hash_after      TEXT,
    details         TEXT                     -- human-readable description
);

CREATE INDEX IF NOT EXISTS idx_log_uuid ON sync_log(uuid);
CREATE INDEX IF NOT EXISTS idx_log_timestamp ON sync_log(timestamp);

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY
);
```

### 2. Rust interface

```rust
pub struct StateDb {
    conn: rusqlite::Connection,
}

impl StateDb {
    /// Open or create the database at the given path. Run migrations.
    pub fn open(db_path: &Path) -> Result<Self>

    /// Run schema migrations. Idempotent.
    pub fn migrate(&self) -> Result<()>

    // --- sync_state CRUD ---

    /// Insert or update a sync state record.
    pub fn upsert_state(&self, state: &SyncFileState) -> Result<()>

    /// Get the sync state for a single UUID.
    pub fn get_state(&self, uuid: &str) -> Result<Option<SyncFileState>>

    /// Get all sync state records.
    pub fn get_all_states(&self) -> Result<Vec<SyncFileState>>

    /// Get all records with a specific status.
    pub fn get_by_status(&self, status: SyncStatus) -> Result<Vec<SyncFileState>>

    /// Delete a sync state record (file was removed from both sides).
    pub fn delete_state(&self, uuid: &str) -> Result<()>

    /// Mark a file as successfully synced (copy current hashes to synced columns).
    pub fn mark_synced(&self, uuid: &str) -> Result<()>

    /// Mark a file as conflicted, storing conflict details.
    pub fn mark_conflict(&self, uuid: &str, info: &str) -> Result<()>

    // --- sync_log ---

    /// Record a sync action in the log.
    pub fn log_action(&self, entry: &SyncLogEntry) -> Result<()>

    /// Get the last N sync log entries.
    pub fn get_recent_log(&self, limit: u32) -> Result<Vec<SyncLogEntry>>

    /// Get sync history for a specific document.
    pub fn get_log_for_uuid(&self, uuid: &str) -> Result<Vec<SyncLogEntry>>
}
```

### 3. Data structs

```rust
#[derive(Debug, Clone)]
pub struct SyncFileState {
    pub uuid: String,
    pub visible_name: String,
    pub parent_uuid: String,
    pub doc_type: String,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    pub synced_hash: Option<String>,
    pub local_mtime: Option<u64>,
    pub remote_mtime: Option<u64>,
    pub synced_mtime: Option<u64>,
    pub last_sync_at: Option<u64>,
    pub sync_status: SyncStatus,
    pub conflict_info: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncStatus {
    Pending,
    Synced,
    Conflict,
    Error,
}

#[derive(Debug, Clone)]
pub struct SyncLogEntry {
    pub uuid: String,
    pub action: String,
    pub direction: Option<String>,
    pub timestamp: u64,
    pub hash_before: Option<String>,
    pub hash_after: Option<String>,
    pub details: Option<String>,
}
```

### 4. Transaction safety

- `mark_synced` should update `synced_hash = local_hash`, `synced_mtime = local_mtime`, `sync_status = 'synced'`, and `last_sync_at = now()` in a single transaction.
- Batch operations (syncing many files) should wrap all state updates in a single transaction for atomicity.
- Provide `pub fn transaction<F, T>(&self, f: F) -> Result<T>` for callers to wrap batch operations.

## Files to Create/Modify

- `src/sync/state_db.rs` — full implementation
- `src/sync/mod.rs` — export the module

## Test Strategy

All tests use an in-memory SQLite database (`:memory:`):

1. **Open and migrate** — verify tables are created.
2. **Upsert and get** — insert a record, retrieve it, verify all fields.
3. **Mark synced** — insert a record with different local/remote hashes, call `mark_synced`, verify synced columns update.
4. **Mark conflict** — verify status changes and conflict_info is stored.
5. **Delete** — insert then delete, verify `get_state` returns None.
6. **Log action** — write 3 log entries, retrieve recent 2, verify order (newest first).
7. **Get by status** — insert records with mixed statuses, filter by `Conflict`, verify only matching records return.
8. **Idempotent migration** — call `migrate()` twice, verify no error.

## Acceptance Criteria

1. `StateDb::open` creates the database file and runs migrations.
2. All CRUD operations work correctly.
3. `mark_synced` atomically updates synced columns.
4. Transaction wrapper commits on success, rolls back on error.
5. Schema migration is idempotent.
6. All unit tests pass with in-memory database.
