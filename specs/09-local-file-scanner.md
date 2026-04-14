# Spec 09 — Local File Scanner

**Layer:** 2 — Local Infrastructure  
**Dependencies:** 01 (project scaffolding), 02 (metadata parser), 05 (SQLite state)  
**Estimated effort:** 1–2 hours  

## Objective

Implement a scanner that inventories the local sync directory, parses reMarkable metadata files, computes content hashes, and produces a local manifest comparable to the remote scanner's output.

## Context

The local sync directory at `{sync_destination}/raw/` mirrors the reMarkable's xochitl filesystem. This scanner walks that directory, parses metadata, hashes file contents, and returns a snapshot of the local state. The sync engine diffs this against the remote manifest and the SQLite baseline to determine what changed.

## Technical Requirements

### 1. Local scanner (`src/sync/scanner.rs` — extend existing file)

```rust
/// A snapshot of a single document stored locally.
#[derive(Debug, Clone)]
pub struct LocalDocumentSnapshot {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub content_hash: String,           // SHA-256 over all local files for this UUID
    pub total_size_bytes: u64,
    pub mtime: u64,                     // Most recent mtime across all local files
    pub page_count: usize,
    pub has_pdf: bool,
    pub file_list: Vec<LocalFileInfo>,
}

#[derive(Debug, Clone)]
pub struct LocalFileInfo {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

/// A complete snapshot of the local sync directory.
#[derive(Debug)]
pub struct LocalManifest {
    pub documents: Vec<LocalDocumentSnapshot>,
    pub scanned_at: u64,
    pub total_documents: usize,
    pub total_size_bytes: u64,
    pub sync_dir: PathBuf,
}
```

### 2. Scanner implementation

```rust
/// Scan the local sync directory and produce a complete manifest.
pub fn scan_local(sync_dir: &Path) -> Result<LocalManifest>
```

The `sync_dir` is the user-selected sync destination. The raw reMarkable files live under `{sync_dir}/raw/`.

Implementation steps:

1. Verify `{sync_dir}/raw/` exists. If not, create it and return an empty manifest.
2. Scan for all `*.metadata` files in `raw/`.
3. For each UUID:
   a. Parse `.metadata` — skip deleted items.
   b. Parse `.content` if present.
   c. Walk the `{UUID}/` subdirectory for `.rm` page files.
   d. Stat all files — collect sizes and mtimes.
   e. Compute content hash (SHA-256, same algorithm as remote scanner for comparability).
4. Build and return `LocalManifest`.

### 3. Content hashing (must match remote hashing)

```rust
/// Compute a deterministic hash for a local document bundle.
/// MUST use the same algorithm as the remote scanner so hashes are comparable.
pub fn compute_local_hash(uuid: &str, files: &[LocalFileInfo], raw_dir: &Path) -> Result<String>
```

- Read file contents for files < 1MB, hash them.
- For files >= 1MB, hash `"{relative_path}:{size}:{mtime}"`.
- Sort file paths alphabetically before hashing.
- Use SHA-256, output as lowercase hex string.

### 4. File watching integration point

Add a hook for the `notify` crate's file watcher. This spec doesn't implement continuous watching — it just provides the interface:

```rust
/// Returns the list of paths that should be watched for changes.
/// Used by the file watcher to know what to monitor.
pub fn get_watch_paths(sync_dir: &Path) -> Vec<PathBuf>
```

This returns `[{sync_dir}/raw/]` — the watcher (a future spec or enhancement) will monitor this recursively.

### 5. Comparison helper

```rust
/// Compare a local and remote snapshot of the same UUID.
/// Returns the change type detected.
#[derive(Debug, PartialEq)]
pub enum ChangeType {
    Unchanged,
    ModifiedLocally,
    ModifiedRemotely,
    ModifiedBoth,    // Conflict
    NewLocal,        // Exists locally but not in synced state
    NewRemote,       // Exists remotely but not in synced state
    DeletedLocally,  // In synced state but gone locally
    DeletedRemotely, // In synced state but gone remotely
}

pub fn classify_change(
    local: Option<&LocalDocumentSnapshot>,
    remote: Option<&RemoteDocumentSnapshot>,
    synced: Option<&SyncFileState>,
) -> ChangeType
```

## Files to Create/Modify

- `src/sync/scanner.rs` — add local scanner types and implementation
- `src/sync/mod.rs` — export new types

## Test Strategy

1. **Empty directory** — scan a directory with no `.metadata` files, verify empty manifest.
2. **Single document** — create a temp dir with one UUID's worth of files (.metadata, .content, fake .rm), scan it, verify snapshot fields.
3. **Hash consistency** — scan the same directory twice, verify hashes are identical.
4. **Hash changes on modification** — modify a file's content, rescan, verify hash changes.
5. **Deleted documents excluded** — include a `.metadata` with `deleted: true`, verify it's excluded.
6. **classify_change tests**:
   - Local and remote hashes match synced → `Unchanged`
   - Local hash differs from synced, remote matches → `ModifiedLocally`
   - Remote hash differs from synced, local matches → `ModifiedRemotely`
   - Both differ from synced → `ModifiedBoth`
   - No synced state exists, local exists → `NewLocal`
   - No synced state exists, remote exists → `NewRemote`

## Acceptance Criteria

1. `scan_local` produces accurate manifests from a local sync directory.
2. Content hashes are deterministic and use the same algorithm as the remote scanner.
3. `classify_change` correctly categorizes all change types.
4. Deleted items are excluded.
5. Missing `raw/` directory is created automatically.
6. All unit tests pass.
