# Spec 24 — End-to-End Integration Tests

**Layer:** 6 — Integration  
**Dependencies:** All previous specs  
**Estimated effort:** 2 hours  

## Objective

Build a test harness with mock device support that validates the complete sync pipeline from scan through transfer, and the document viewing pipeline from .rm parsing through SVG rendering.

## Context

Individual specs have unit tests for their components. This spec creates integration tests that exercise the full flow without requiring a physical reMarkable. It uses a mock SFTP server (or mock connection layer) and pre-built test fixtures.

## Technical Requirements

### 1. Test fixtures directory

Create `tests/fixtures/` with synthetic reMarkable data:

```
tests/fixtures/
├── mock_xochitl/                    # Simulates the device filesystem
│   ├── abc123.metadata              # A notebook
│   ├── abc123.content
│   ├── abc123/
│   │   ├── page1.rm                 # Minimal valid .rm v6 binary
│   │   └── page2.rm
│   ├── def456.metadata              # A folder
│   ├── ghi789.metadata              # A PDF document
│   ├── ghi789.content
│   ├── ghi789.pdf                   # Small test PDF
│   └── deleted1.metadata            # Deleted document (parent: "trash")
└── mock_local/                      # Simulates a local sync directory
    └── raw/
        ├── abc123.metadata          # Slightly older version than mock_xochitl
        ├── abc123.content
        └── abc123/
            ├── page1.rm
            └── page2.rm
```

### 2. Mock connection layer

```rust
/// A mock DeviceConnection that reads from a local directory
/// instead of connecting over SSH/SFTP.
pub struct MockDeviceConnection {
    root_dir: PathBuf,  // Points to tests/fixtures/mock_xochitl/
}

impl MockDeviceConnection {
    pub fn new(fixture_dir: &Path) -> Self

    // Implement the same interface as DeviceConnection:
    pub async fn list_dir(&self, path: &str) -> Result<Vec<RemoteFileInfo>>
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>>
    pub async fn stat_file(&self, path: &str) -> Result<RemoteFileInfo>
    pub async fn download_file(&self, remote: &str, local: &Path) -> Result<u64>
    pub async fn upload_file(&self, local: &Path, remote: &str) -> Result<u64>
    pub async fn write_file(&self, path: &str, data: &[u8]) -> Result<()>
    pub async fn delete_file(&self, path: &str) -> Result<()>
    pub async fn mkdir(&self, path: &str) -> Result<()>
    pub fn ping(&self) -> bool { true }
}
```

Alternatively, define a `trait DeviceTransport` that both `DeviceConnection` and `MockDeviceConnection` implement, and make the sync engine generic over this trait.

### 3. Integration test: first sync (pull all)

```rust
#[tokio::test]
async fn test_first_sync_pulls_all_documents() {
    // Setup: mock device with 2 documents, empty local dir, empty SQLite
    // Act: run full sync
    // Assert:
    //   - Both documents appear in local raw/ directory
    //   - SQLite has 2 entries with status "synced"
    //   - Sync report shows 2 pulled, 0 pushed
}
```

### 4. Integration test: bidirectional sync

```rust
#[tokio::test]
async fn test_bidirectional_sync() {
    // Setup:
    //   - Document A: modified remotely (remote hash != synced hash, local matches synced)
    //   - Document B: modified locally (local hash != synced hash, remote matches synced)
    //   - Document C: unchanged on both sides
    // Act: run sync
    // Assert:
    //   - A is pulled (remote → local)
    //   - B is pushed (local → remote)
    //   - C is skipped
    //   - All three are "synced" in SQLite
}
```

### 5. Integration test: conflict resolution

```rust
#[tokio::test]
async fn test_conflict_creates_backup() {
    // Setup:
    //   - Document A: different modifications on both sides (both hashes differ from synced)
    //   - Remote has newer mtime
    // Act: run sync
    // Assert:
    //   - Remote version wins (pulled)
    //   - Local version saved as backup with "(conflict {date})" name
    //   - Backup has a new UUID
    //   - SQLite has entries for both original and backup
}
```

### 6. Integration test: render pipeline

```rust
#[test]
fn test_full_render_pipeline() {
    // Setup: fixture .rm file
    // Act:
    //   1. parse_rm_file
    //   2. render_page_to_svg
    //   3. Verify SVG is valid XML
    //   4. Verify SVG has viewBox="0 0 1404 1872"
    //   5. Verify SVG contains expected stroke elements
}
```

### 7. Integration test: document tree from synced files

```rust
#[test]
fn test_document_tree_from_sync_dir() {
    // Setup: mock_local fixture
    // Act: scan_local → DocumentTree::build_from_directory
    // Assert:
    //   - Tree has correct structure
    //   - Folder hierarchy is reconstructed
    //   - Deleted items excluded
}
```

### 8. Generate .rm test fixture

Write a small helper that generates a minimal valid .rm v6 binary file:

```rust
/// Create a minimal .rm v6 file with a single stroke for testing.
pub fn create_test_rm_file(output_path: &Path) -> Result<()> {
    let mut data = Vec::new();
    // Header
    data.extend_from_slice(b"reMarkable .lines file, version=6          ");
    // ... padding ...
    // 1 layer
    data.extend_from_slice(&1_i32.to_le_bytes());
    // 1 stroke
    data.extend_from_slice(&1_i32.to_le_bytes());
    // Pen type: fineliner (2)
    data.extend_from_slice(&2_i32.to_le_bytes());
    // Color: black (0)
    data.extend_from_slice(&0_i32.to_le_bytes());
    // ... remaining stroke fields and points ...
    std::fs::write(output_path, &data)?;
    Ok(())
}
```

## Files to Create/Modify

- `tests/fixtures/` — create all fixture files
- `tests/integration_sync.rs` — sync pipeline integration tests
- `tests/integration_render.rs` — render pipeline integration tests
- `tests/common/mod.rs` — shared test helpers (MockDeviceConnection, fixture builder)
- `src/device/connection.rs` — optionally extract `DeviceTransport` trait

## Acceptance Criteria

1. All integration tests pass without a physical reMarkable connected.
2. First-sync scenario correctly pulls all documents.
3. Bidirectional scenario correctly identifies and executes pull/push.
4. Conflict scenario creates backup and preserves both versions.
5. Render pipeline produces valid SVG from .rm binary fixtures.
6. Document tree reconstructs correctly from synced files.
7. Tests run in CI (no hardware dependency).
