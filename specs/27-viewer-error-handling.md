# Spec 27 — Viewer Error Handling & Diagnostic Feedback

**Layer:** 5 — Viewer  
**Dependencies:** 26 (content v2 parser), 20/21 (viewer)  
**Estimated effort:** 1 hour  
**Priority:** High — user sees a blank pane with no explanation  

## Objective

When document loading fails for any reason — missing files, parse errors, unsupported format versions, rendering failures — surface a clear, actionable message in the viewer pane instead of leaving the user staring at an empty placeholder.

## Context

The current viewer pipeline has multiple silent-failure paths:

| Failure | Current behaviour | User sees |
|---------|------------------|-----------|
| `.content` missing or unparseable | Error returned from `load_document`, caught in `app.rs:130`, logged via `tracing::warn!` | Blank viewer, no indication |
| `.rm` file missing for a page | `if !rm_path.exists() { continue; }` — silently skipped | Some pages may render, others blank, or all blank if every page is missing |
| `.rm` parse failure (wrong version, corrupt data) | Error propagated via `?`, caught in `app.rs:130`, logged | Blank viewer |
| SVG cache write failure | Error propagated via `?` | Blank viewer |
| `GtkPicture::for_filename` on non-existent SVG | GTK defers load to paint-time; file-not-found → silent blank | Blank picture widget |
| librsvg not installed | GTK pixbuf loader can't decode SVG → silent blank | Blank picture widget |

In every case the user gets no feedback in the application UI.

## Technical Requirements

### 1. Inline error display in the viewer pane

When `load_document` fails or produces zero renderable pages, show a descriptive message **inside the viewer pane** (not in a dialog — the user should be able to click a different document without dismissing anything).

```rust
impl DocumentViewer {
    /// Show an error message inside the viewer pane.
    fn show_error(&self, heading: &str, detail: &str)
    
    /// Show a partial-load warning below rendered pages.
    fn show_partial_warning(&self, rendered: usize, total: usize, failures: &[String])
}
```

Use a dedicated `GtkStack` page called `"error"` containing:
```
[Warning icon]
Could not display "Meeting Notes"

3 of 5 pages failed to render:
  • page 022c0e2a: unsupported .rm version 5
  • page 385d6f52: file not found
  • page 333f19ef: file not found
```

### 2. Resilient page loop

Instead of returning early on the first error, `load_document` should attempt every page and accumulate results:

```rust
enum PageResult {
    Rendered { cache_path: PathBuf },
    Missing { page_id: String },
    ParseFailed { page_id: String, reason: String },
    CacheFailed { page_id: String, reason: String },
}
```

After the loop:
- If ALL pages rendered → show pages normally.
- If SOME pages rendered → show pages + a bottom banner noting N failures.
- If ZERO pages rendered → show the error pane with the list of reasons.

### 3. Surface errors from `app.rs` wiring

In `wire_folder_browser_to_viewer`, replace the `tracing::warn!` fallback with a call to `viewer.show_error(...)`:

```rust
browser.connect_document_selected(move |uuid| {
    let sync_dir = config.borrow().sync.sync_dir.clone();
    match viewer.load_document(&uuid, &sync_dir) {
        Ok(()) => {}
        Err(e) => viewer.show_error(
            &format!("Could not open document"),
            &e.to_string(),
        ),
    }
});
```

### 4. SVG render fallback

If `GtkPicture::for_filename` is suspected of failing silently (librsvg missing, file doesn't exist), add a pre-check:

```rust
fn build_page_widget(svg_path: &Path, page_number: usize) -> gtk::Box {
    if !svg_path.exists() {
        return build_error_page_widget(page_number, "SVG cache file missing");
    }
    // ... existing GtkPicture code ...
}
```

Optionally: use `resvg` (already a dependency) to render SVG → PNG in-memory, then feed the `gdk::MemoryTexture` directly to the picture. This eliminates the runtime dependency on librsvg and makes rendering fully self-contained.

## Files to Create/Modify

- `src/ui/viewer.rs` — add error/partial-warning display, resilient page loop, SVG pre-check
- `src/app.rs` — replace `tracing::warn!` with `viewer.show_error`

## Test Strategy

1. **Missing .content file** — call `load_document` with a UUID that has no `.content` file, verify error pane shows with "file not found" message.
2. **All pages missing** — create a `.content` with 3 page UUIDs but no `.rm` files, verify error pane lists all 3 as missing.
3. **Partial render** — 3 pages, 2 present and 1 missing, verify 2 pages render with a warning banner.
4. **Parse error** — create a `.rm` file with a garbage header, verify the failure appears in the error list, other pages still render.
5. **SVG file missing from cache** — delete a cached `.svg`, reload, verify page re-renders or shows a targeted error.

## Acceptance Criteria

1. The viewer never shows a blank, unexplained pane — every failure has a visible message.
2. Partial documents render what they can, with a clear count of failures.
3. The error pane names the specific reason per page (missing, parse error, version mismatch).
4. Clicking a different document in the sidebar correctly replaces the error pane with content or a new error.
5. No error dialogs requiring dismissal — everything is inline.
