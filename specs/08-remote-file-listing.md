# Spec 08 — Remote File Scanner

**Layer:** 1 — Connectivity  
**Dependencies:** 06 (SSH/SFTP module), 02 (metadata parser)  
**Estimated effort:** 1–2 hours  

## Objective

Implement a scanner that connects to the reMarkable via SFTP, inventories all documents in the xochitl directory, parses their metadata, and returns a structured manifest of the device's contents with content hashes.

## Context

The reMarkable stores all documents at `/home/root/.local/share/remarkable/xochitl/`. Each document is a UUID with associated files (.metadata, .content, .rm pages, etc.). The remote scanner reads this directory over SFTP, parses the metadata, and computes content hashes — producing a snapshot of the device's current state for the sync engine to diff against.

## Technical Requirements

### 1. Remote scanner (`src/sync/scanner.rs`)

```rust
/// A snapshot of a single document on the reMarkable.
#[derive(Debug, Clone)]
pub struct RemoteDocumentSnapshot {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub content_hash: String,           // SHA-256 over all .rm files + .metadata + .content
    pub total_size_bytes: u64,          // Sum of all file sizes for this document
    pub mtime: u64,                     // Most recent mtime across all files
    pub page_count: usize,             // Number of .rm page files
    pub has_pdf: bool,                  // Whether a source .pdf exists
    pub file_list: Vec<RemoteFileInfo>, // All files belonging to this UUID
}

/// A complete snapshot of the device's document state.
#[derive(Debug)]
pub struct RemoteManifest {
    pub documents: Vec<RemoteDocumentSnapshot>,
    pub scanned_at: u64,                // Unix timestamp when scan completed
    pub total_documents: usize,
    pub total_size_bytes: u64,
}

pub const XOCHITL_PATH: &str = "/home/root/.local/share/remarkable/xochitl";
```

### 2. Scanner implementation

```rust
/// Scan the reMarkable device and produce a complete manifest.
pub async fn scan_remote(conn: &DeviceConnection) -> Result<RemoteManifest>
```

Implementation steps:

1. `conn.list_dir(XOCHITL_PATH)` — get all files and directories.
2. Identify unique UUIDs by finding all `*.metadata` files (strip the extension to get the UUID).
3. For each UUID:
   a. Read and parse the `.metadata` file via `conn.read_file()` + `serde_json::from_slice()`.
   b. Skip if `deleted == true` or `parent == "trash"`.
   c. Read and parse `.content` if it exists.
   d. List the UUID's subdirectory (if it exists) to find `.rm` page files.
   e. Stat all files belonging to this UUID to get sizes and mtimes.
   f. Compute a content hash: sort all file paths alphabetically, concatenate their contents (or stat data for large files), SHA-256 the result.
   g. Build a `RemoteDocumentSnapshot`.
4. Return the assembled `RemoteManifest`.

### 3. Content hashing strategy

For hash computation per document:

```rust
fn compute_remote_hash(
    conn: &DeviceConnection,
    uuid: &str,
    file_list: &[RemoteFileInfo],
) -> Result<String>
```

- For small files (<1MB): read contents, feed into SHA-256.
- For large files (PDFs, etc.): use `size + mtime` as a proxy to avoid downloading entire files just for hashing. Hash the string `"{path}:{size}:{mtime}"` for each file.
- Sort file paths before hashing for deterministic results.
- The hash should change if any file in the document bundle changes.

### 4. Progress reporting

The scan can be slow over SFTP (many small file reads). Provide progress:

```rust
pub async fn scan_remote_with_progress<F>(
    conn: &DeviceConnection,
    progress_callback: F,
) -> Result<RemoteManifest>
where
    F: Fn(ScanProgress) + Send + 'static,

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub current: usize,
    pub total: usize,
    pub current_name: String,
}
```

### 5. Performance considerations

- Batch SFTP operations where possible — reuse the session, don't reconnect per file.
- Read `.metadata` files first (they're tiny) to build the inventory before reading larger content.
- Use `stat` instead of `read` for file sizes/mtimes when full content isn't needed.
- Consider parallel reads (2–4 concurrent SFTP reads) if `russh-sftp` supports it.

## Files to Create/Modify

- `src/sync/scanner.rs` — add `scan_remote`, `scan_remote_with_progress`, and supporting types
- `src/sync/mod.rs` — export new types

## Test Strategy

1. **Parse mock metadata from bytes** — create a JSON byte array, deserialize, verify fields.
2. **Hash determinism** — given the same file list and content, verify hash is identical across calls.
3. **Hash changes on content change** — modify one file's mtime, verify hash changes.
4. **Skip deleted documents** — include a deleted document in mock data, verify it's excluded from manifest.
5. **Progress callback** — verify callback is called with incrementing current values.

Integration test (with device):
6. **Full scan** — connect to a reMarkable, run `scan_remote`, verify manifest contains expected documents.

## Acceptance Criteria

1. `scan_remote` returns a complete manifest of all non-deleted documents on the device.
2. Each document has a deterministic content hash.
3. Metadata and content are correctly parsed from remote files.
4. Progress callback fires for each document processed.
5. Deleted/trashed documents are excluded.
6. All unit tests pass.
