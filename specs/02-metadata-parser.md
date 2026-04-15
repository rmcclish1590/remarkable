# Spec 02 — reMarkable Metadata Parser

**Layer:** 0 — Foundation  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 1–2 hours  

## Objective

Implement the parser for reMarkable `.metadata` and `.content` JSON files, and build a `DocumentTree` model that reconstructs the tablet's folder hierarchy from flat UUID-based files.

## Context

On the reMarkable 2, every document and folder is identified by a UUID. The filesystem at `/home/root/.local/share/remarkable/xochitl/` contains flat files like `{UUID}.metadata` and `{UUID}.content`. The folder hierarchy is virtual — encoded through `parent` UUID references in the metadata JSON. This parser must reconstruct that tree.

## Technical Requirements

### 1. Metadata struct (`src/remarkable/metadata.rs`)

Parse `.metadata` JSON files. Example file content:

```json
{
    "deleted": false,
    "lastModified": "1712934567890",
    "lastOpened": "1712934567890",
    "lastOpenedPage": 3,
    "metadatamodified": false,
    "modified": false,
    "parent": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "pinned": false,
    "synced": true,
    "type": "DocumentType",
    "version": 1,
    "visibleName": "Meeting Notes"
}
```

Define a serde-deserializable struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableMetadata {
    pub deleted: bool,
    #[serde(rename = "lastModified")]
    pub last_modified: String,        // Unix timestamp as string (milliseconds)
    #[serde(rename = "lastOpened")]
    pub last_opened: Option<String>,
    #[serde(rename = "lastOpenedPage")]
    pub last_opened_page: Option<u32>,
    pub metadatamodified: Option<bool>,
    pub modified: Option<bool>,
    pub parent: String,               // UUID of parent folder, "" for root, "trash" for deleted
    pub pinned: bool,
    pub synced: Option<bool>,
    #[serde(rename = "type")]
    pub doc_type: String,             // "DocumentType" or "CollectionType"
    pub version: Option<u32>,
    #[serde(rename = "visibleName")]
    pub visible_name: String,
}
```

Implement:
- `RemarkableMetadata::from_file(path: &Path) -> Result<Self>`
- `RemarkableMetadata::is_folder(&self) -> bool` (true if `doc_type == "CollectionType"`)
- `RemarkableMetadata::is_document(&self) -> bool`
- `RemarkableMetadata::is_deleted(&self) -> bool` (true if `deleted` or `parent == "trash"`)
- `RemarkableMetadata::last_modified_ms(&self) -> Result<u64>` (parse the string timestamp)

### 2. Content struct (`src/remarkable/metadata.rs`)

Parse `.content` JSON files. Example:

```json
{
    "dpiRasterBackground": 226,
    "fileType": "notebook",
    "formatVersion": 2,
    "orientation": "portrait",
    "pageCount": 5,
    "pages": [
        "uuid-page-1",
        "uuid-page-2",
        "uuid-page-3",
        "uuid-page-4",
        "uuid-page-5"
    ],
    "textAlignment": "justify",
    "textScale": 1
}
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableContent {
    #[serde(rename = "fileType")]
    pub file_type: Option<String>,     // "notebook", "pdf", "epub"
    #[serde(rename = "formatVersion")]
    pub format_version: Option<u32>,
    pub orientation: Option<String>,    // "portrait" or "landscape"
    #[serde(rename = "pageCount")]
    pub page_count: Option<u32>,
    pub pages: Option<Vec<String>>,     // Ordered list of page UUIDs
    #[serde(rename = "textScale")]
    pub text_scale: Option<f64>,
}
```

Implement `RemarkableContent::from_file(path: &Path) -> Result<Self>`.

### 3. Document tree model (`src/remarkable/document.rs`)

```rust
#[derive(Debug, Clone)]
pub struct DocumentNode {
    pub uuid: String,
    pub metadata: RemarkableMetadata,
    pub content: Option<RemarkableContent>,
    pub children: Vec<DocumentNode>,    // Only populated for folders
}

#[derive(Debug)]
pub struct DocumentTree {
    pub roots: Vec<DocumentNode>,       // Top-level items (parent == "")
}
```

Implement:
- `DocumentTree::build_from_directory(dir: &Path) -> Result<Self>`:
  1. Scan the directory for all `*.metadata` files.
  2. Parse each one, keyed by UUID (filename stem).
  3. Optionally parse matching `.content` files.
  4. Filter out deleted items.
  5. Build the tree by resolving `parent` references.
  6. Sort children alphabetically by `visible_name` within each folder, folders first.

- `DocumentTree::find_by_uuid(&self, uuid: &str) -> Option<&DocumentNode>`
- `DocumentTree::flat_list(&self) -> Vec<&DocumentNode>` — returns all documents (not folders) in tree order.

### 4. Handle edge cases

- Missing `.content` file: valid — some items (folders) don't have one. Set `content: None`.
- Orphaned documents: if a document's `parent` UUID doesn't exist, place it at root level.
- `parent == "trash"`: exclude from the tree (treat as deleted).
- `lastModified` as string: the reMarkable stores this as a string, not a number. Parse it.

## Files to Create/Modify

- `src/remarkable/metadata.rs` — full implementation
- `src/remarkable/document.rs` — full implementation
- `src/remarkable/mod.rs` — export both modules

## Test Strategy

Create `tests/metadata_test.rs` (or inline `#[cfg(test)]` modules):

1. **Parse valid metadata JSON** — create a sample JSON string, deserialize, verify all fields.
2. **Parse valid content JSON** — same approach.
3. **Build tree from test directory** — create a temp directory with 3–5 mock `.metadata` and `.content` files representing a folder with nested documents. Verify the tree structure is correct.
4. **Orphaned document** — a document whose parent UUID doesn't exist should appear at root.
5. **Deleted items** — documents with `deleted: true` or `parent: "trash"` should be excluded.

## Acceptance Criteria

1. `RemarkableMetadata::from_file` correctly parses all fields from a `.metadata` file.
2. `RemarkableContent::from_file` correctly parses a `.content` file.
3. `DocumentTree::build_from_directory` produces a correctly nested tree from flat UUID files.
4. Deleted and trashed items are excluded from the tree.
5. Orphaned items appear at root level rather than being lost.
6. All unit tests pass.
