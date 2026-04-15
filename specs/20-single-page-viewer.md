# Spec 20 — Single-Page Document Viewer

**Layer:** 5 — Viewer  
**Dependencies:** 03 (rm parser), 04 (SVG renderer), 15 (main window)  
**Estimated effort:** 1–2 hours  

## Objective

Implement a document viewer that renders a single page of a reMarkable notebook as SVG and displays it in the main content area, scaled to fit the panel width while maintaining aspect ratio and consistent text sizing.

## Context

When the user clicks a document in the sidebar, the viewer must parse its `.rm` page files, render them to SVG, and display the result. This spec handles single-page rendering. Spec 21 extends it to multi-page scrollable viewing.

## Technical Requirements

### 1. Viewer widget (`src/ui/viewer.rs`)

```rust
pub struct DocumentViewer {
    pub widget: gtk::Box,              // Root container
    scroll_window: gtk::ScrolledWindow,
    content_box: gtk::Box,             // Vertical box for page stack
    page_info_label: gtk::Label,       // "Page 1 of 5"
    nav_box: gtk::Box,                 // Page navigation controls
    prev_button: gtk::Button,
    next_button: gtk::Button,
    current_doc: Option<LoadedDocument>,
}

struct LoadedDocument {
    uuid: String,
    name: String,
    pages: Vec<RmPage>,               // Parsed pages
    rendered_svgs: Vec<Option<Vec<u8>>>, // Cached rendered SVG bytes (None = not yet rendered)
    total_pages: usize,
}
```

### 2. Loading a document

```rust
impl DocumentViewer {
    pub fn new() -> Self

    /// Load and display a document from the local sync directory.
    /// Parses metadata, content, and .rm files.
    pub fn load_document(&self, uuid: &str, sync_dir: &Path) -> Result<()>

    /// Clear the viewer (no document selected).
    pub fn clear(&self)

    /// Get the currently loaded document UUID.
    pub fn current_uuid(&self) -> Option<&str>
}
```

`load_document` implementation:

1. Read `{sync_dir}/raw/{uuid}.content` to get the page list and order.
2. For each page UUID in the content's `pages` array:
   a. Check SVG cache at `{sync_dir}/.rmsync/cache/{uuid}_{page_uuid}.svg`.
   b. If cached: load the cached SVG.
   c. If not cached:
      - Read `{sync_dir}/raw/{uuid}/{page_uuid}.rm`.
      - Parse with `parse_rm_file()`.
      - Render with `render_page_to_svg()`.
      - Write SVG to cache.
3. Display the first page.

### 3. SVG display

Use `GtkPicture` (preferred for SVG) or `GtkImage` to render the SVG:

```rust
fn display_page_svg(&self, svg_bytes: &[u8]) -> Result<()> {
    // Option A: Use resvg to render SVG → PNG pixbuf, display via GtkPicture
    // Option B: Write SVG to temp file, load via GtkPicture::for_filename
    // Option C: Use GdkTexture from bytes
    
    // Whichever approach, the image should:
    // 1. Scale to fit the viewer panel width
    // 2. Maintain aspect ratio (1404:1872 = 3:4)
    // 3. Use GtkPicture's content-fit property
}
```

**Recommended approach:** Use `resvg` to render SVG to a pixel buffer at the appropriate resolution, then create a `gdk::Texture` from the pixels and set it on a `GtkPicture`:

```rust
let tree = resvg::Tree::from_data(svg_bytes, &resvg::Options::default())?;
let size = tree.size();
let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width() as u32, size.height() as u32)?;
tree.render(resvg::Transform::default(), &mut pixmap.as_mut());
let bytes = glib::Bytes::from(pixmap.data());
let texture = gdk::MemoryTexture::new(
    pixmap.width() as i32,
    pixmap.height() as i32,
    gdk::MemoryFormat::R8g8b8a8Premultiplied,
    &bytes,
    pixmap.width() as usize * 4,
);
picture.set_paintable(Some(&texture));
```

### 4. Consistent sizing

**Critical requirement:** All pages must render at the same viewport (1404×1872). The `GtkPicture` should be set to `content_fit = gtk::ContentFit::Contain` so it scales to fill the available width while maintaining aspect ratio. This ensures text appears at the same visual size regardless of which document is open.

```rust
picture.set_content_fit(gtk::ContentFit::Contain);
picture.set_can_shrink(true);
```

### 5. Page navigation (single-page mode)

Below the rendered page, show navigation controls:

```
                 Page 3 of 12          [◀ Prev] [Next ▶]
```

- Prev/Next buttons navigate between pages.
- Page counter label updates on navigation.
- Prev is disabled on page 1, Next is disabled on last page.
- Keyboard shortcuts: Left/Right arrows for page navigation.

### 6. Empty state

When no document is loaded, show a centered placeholder:

```
     Select a document from the sidebar to view it
```

Use a dimmed `GtkLabel` centered in the viewer area.

### 7. PDF documents

For documents that are imported PDFs (`.metadata` has `fileType: "pdf"`):
- If the document has `.rm` overlay pages, render the .rm strokes as before (the PDF is the background — handling PDF compositing is a future enhancement).
- If no `.rm` files exist, display a message: "PDF document — view in external viewer" with a button to open the PDF in the system's default PDF viewer.

## Files to Create/Modify

- `src/ui/viewer.rs` — full implementation
- `src/ui/window.rs` — replace viewer placeholder, wire to folder browser selection
- `src/ui/mod.rs` — export module

## Test Strategy

1. **Empty state** — no document loaded, verify placeholder text is shown.
2. **Load notebook** — create mock .rm files in a temp dir, call `load_document`, verify an image appears.
3. **Page navigation** — load a 3-page document, verify prev/next navigate correctly, buttons disable at boundaries.
4. **Consistent sizing** — load two documents, verify rendered images have the same pixel width.
5. **SVG caching** — load a document twice, verify cache files exist and second load is faster.
6. **Clear** — load a document, call `clear()`, verify placeholder returns.

## Acceptance Criteria

1. Selecting a document in the sidebar renders the first page in the viewer.
2. Pages render at consistent sizes (1404×1872 viewport, scaled to fit).
3. Page navigation works with buttons and keyboard arrows.
4. SVG rendering is cached to disk for performance.
5. Empty and PDF states are handled gracefully.
6. Page counter accurately reflects current position.
