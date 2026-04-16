# Spec 26 — Content File v2 Parser (cPages Support)

**Layer:** 0 — Foundation  
**Dependencies:** 02 (metadata parser), 08 (remote scanner), 09 (local scanner), 20/21 (viewer)  
**Estimated effort:** 1–2 hours  
**Priority:** Critical — blocks all document viewing on real devices  

## Objective

Update the `.content` file parser to handle the modern reMarkable firmware's `cPages` format, which nests page metadata inside structured objects rather than a flat string array. Without this fix, every document's page list resolves to empty and the viewer renders nothing.

## Context

### What the parser expects (old format, firmware ≤ 2.x)

```json
{
    "fileType": "notebook",
    "formatVersion": 2,
    "pageCount": 3,
    "pages": ["page-uuid-1", "page-uuid-2", "page-uuid-3"]
}
```

The current `RemarkableContent` struct deserialises `pages` as `Option<Vec<String>>`.

### What the device actually writes (new format, firmware 3.x+)

```json
{
    "cPages": {
        "pages": [
            {
                "id": "ce6e5a87-fd98-4e85-8eac-42c007ec1d54",
                "idx": { "timestamp": "1:3", "value": "aab" },
                "template": { "timestamp": "1:3", "value": "Blank" }
            },
            {
                "id": "6ad0ff23-e9e4-482d-8a3b-2b5d71e469c4",
                "idx": { "timestamp": "1:2", "value": "ba" },
                "template": { "timestamp": "1:1", "value": "P Lines medium" }
            }
        ],
        "lastOpened": { "timestamp": "1:6", "value": "333f19ef-..." },
        "original": { "timestamp": "0:0", "value": -1 }
    },
    "fileType": "notebook",
    "formatVersion": 2,
    "pageCount": 5,
    "orientation": "portrait"
}
```

In the new format, `pages` at the top level **does not exist**. Page UUIDs live at `cPages.pages[].id`. The top-level `pageCount` still exists and is accurate.

### Impact

Every consumer of `RemarkableContent.pages` gets `None` on modern firmware:
- `DocumentViewer::load_document` (`src/ui/viewer.rs`) → empty page loop → blank viewer
- `scan_remote::build_snapshot` (`src/sync/scanner.rs`) → `page_count = 0`
- `DocumentTree` page-count display in folder browser → shows "0 pages"

## Technical Requirements

### 1. Extend `RemarkableContent` (`src/remarkable/metadata.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemarkableContent {
    #[serde(rename = "fileType")]
    pub file_type: Option<String>,
    #[serde(rename = "formatVersion")]
    pub format_version: Option<u32>,
    pub orientation: Option<String>,
    #[serde(rename = "pageCount")]
    pub page_count: Option<u32>,
    /// Old format: flat array of page UUID strings.
    pub pages: Option<Vec<String>>,
    /// New format (firmware 3.x+): structured page objects.
    #[serde(rename = "cPages")]
    pub c_pages: Option<CPages>,
    #[serde(rename = "textScale")]
    pub text_scale: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CPages {
    pub pages: Option<Vec<CPage>>,
    // Other fields (lastOpened, original, uuids) are device-internal
    // metadata and can be ignored for sync/viewing purposes.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CPage {
    pub id: String,
    // idx, template, scrollTime, verticalScroll are device-internal.
    // Capture them as serde_json::Value to preserve on round-trip
    // without needing to model every field.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
```

### 2. Unified page-ID accessor

```rust
impl RemarkableContent {
    /// Return the ordered list of page UUIDs regardless of content format.
    /// Checks `cPages.pages[].id` first (modern firmware), falls back to
    /// the flat `pages` array (legacy firmware).
    pub fn page_ids(&self) -> Vec<String> {
        if let Some(cp) = &self.c_pages {
            if let Some(pages) = &cp.pages {
                return pages.iter().map(|p| p.id.clone()).collect();
            }
        }
        self.pages.clone().unwrap_or_default()
    }
}
```

### 3. Update all call-sites to use `page_ids()`

Replace every `content.pages.unwrap_or_default()` and similar access with `content.page_ids()`:

- `src/ui/viewer.rs` — `DocumentViewer::load_document` (currently line ~128)
- `src/sync/scanner.rs` — `build_snapshot` (remote scanner, page_count derivation)
- `src/sync/scanner.rs` — `build_local_snapshot` (local scanner, if it reads pages)
- Any test fixtures that construct `RemarkableContent` directly

### 4. Preserve round-trip fidelity

When the sync engine writes `.content` files back to the device (push), the `cPages` structure must be preserved exactly — the `#[serde(flatten)]` on `CPage.extra` ensures fields like `idx`, `template`, `scrollTime`, `verticalScroll` survive serialisation without being modelled explicitly.

## Files to Create/Modify

- `src/remarkable/metadata.rs` — extend `RemarkableContent`, add `CPages`/`CPage`, add `page_ids()`
- `src/ui/viewer.rs` — replace `content.pages.unwrap_or_default()` with `content.page_ids()`
- `src/sync/scanner.rs` — same replacement

## Test Strategy

1. **Parse real v2 content file** — use the verbatim JSON captured from the device (see Context above), verify `page_ids()` returns the 5 UUIDs in order.
2. **Parse legacy content file** — use the existing `CONTENT_SAMPLE` test fixture with flat `pages` array, verify `page_ids()` returns the same 5 UUIDs.
3. **Empty pages / null cPages** — verify `page_ids()` returns `[]`.
4. **Round-trip** — serialise a parsed `RemarkableContent` back to JSON, verify `cPages.pages[].template` etc. survive.
5. **Page count consistency** — verify `page_ids().len() == page_count.unwrap_or(0)` for both formats.

## Acceptance Criteria

1. `page_ids()` returns correct UUIDs from both old and new `.content` formats.
2. `DocumentViewer::load_document` renders pages from a synced notebook using the cPages format.
3. Page counts in the folder browser sidebar match reality.
4. Existing tests still pass (old format is not broken).
5. All new unit tests pass.
