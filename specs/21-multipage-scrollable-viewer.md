# Spec 21 — Multi-Page Scrollable Viewer

**Layer:** 5 — Viewer  
**Dependencies:** 20 (single-page viewer)  
**Estimated effort:** 1–2 hours  

## Objective

Extend the document viewer to display all pages of a notebook as a continuous scrollable stack, with lazy rendering for performance — matching the reMarkable's reading experience where long notes scroll naturally.

## Context

The user's requirement is that "if they are long notes, the sheet should scroll" and "text should stay consistent in size." This spec replaces single-page navigation with a continuous vertical scroll of all pages, like scrolling through a PDF. Pages are rendered lazily — only visible pages (and one buffer page above/below) are rendered to avoid memory/CPU overhead on 100+ page notebooks.

## Technical Requirements

### 1. Extend DocumentViewer (`src/ui/viewer.rs`)

Replace the single-page display with a vertical page stack:

```rust
/// Updated viewer that displays all pages in a scrollable stack.
impl DocumentViewer {
    /// Load and display all pages of a document in scroll mode.
    pub fn load_document(&self, uuid: &str, sync_dir: &Path) -> Result<()>
    
    /// Scroll to a specific page number.
    pub fn scroll_to_page(&self, page_number: usize)
    
    /// Get the currently visible page number (based on scroll position).
    pub fn current_visible_page(&self) -> usize
}
```

### 2. Page stack layout

Inside the `GtkScrolledWindow`, use a vertical `GtkBox` containing one widget per page:

```
GtkScrolledWindow (vertical scroll)
└── GtkBox (vertical, spacing: 16px between pages)
    ├── PageWidget { page: 0, picture: GtkPicture, placeholder: GtkSpinner }
    ├── GtkSeparator (horizontal, thin line between pages)
    ├── PageWidget { page: 1, picture: GtkPicture, placeholder: GtkSpinner }
    ├── GtkSeparator
    ├── PageWidget { page: 2, ... }
    └── ...
```

Each `PageWidget` is a `GtkOverlay` or `GtkStack`:
- **Placeholder state:** Shows a `GtkSpinner` (loading indicator) at the correct page dimensions (using a fixed-size `GtkBox` at 1404:1872 aspect ratio).
- **Rendered state:** Shows the `GtkPicture` with the rendered SVG.

### 3. Lazy rendering

```rust
struct LazyPageManager {
    pages: Vec<PageState>,
    render_buffer: usize,  // Number of pages above/below viewport to pre-render (default: 1)
}

enum PageState {
    Unloaded,                          // Not yet parsed/rendered
    Loading,                           // Currently being rendered in background
    Loaded { texture: gdk::Texture },  // Ready to display
    Cached { cache_path: PathBuf },    // On disk but not in memory
}
```

Implementation:

1. On document load, create `PageWidget` placeholders for ALL pages (so scrollbar size is correct), but don't render any.
2. Connect to the `GtkScrolledWindow`'s `GtkAdjustment` `value-changed` signal.
3. On scroll:
   a. Calculate which pages are visible (based on scroll position and page height).
   b. For visible pages ± `render_buffer`:
      - If `Unloaded`: start async rendering (parse .rm → SVG → texture). Set to `Loading`.
      - If `Cached`: load from disk cache into texture.
   c. For pages far from viewport:
      - If `Loaded`: release the texture from memory, set to `Cached` (keep disk cache).
      - This bounds memory usage.

4. When a page finishes rendering, replace its placeholder with the actual `GtkPicture`.

### 4. Rendering pipeline (background)

Rendering should NOT block the GTK main thread:

```rust
// Spawn on Tokio runtime
tokio::spawn(async move {
    let rm_data = std::fs::read(&rm_path)?;
    let page = parse_rm_file(&rm_data)?;
    let svg = render_page_to_svg(&page);
    
    // Render SVG to pixel buffer
    let tree = resvg::Tree::from_data(svg.as_bytes(), &opts)?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    tree.render(transform, &mut pixmap.as_mut());
    
    // Cache to disk
    std::fs::write(&cache_path, &svg)?;
    
    // Send pixels back to main thread
    sender.send((page_index, pixmap.data().to_vec()))?;
});

// On main thread (via glib channel), create texture and update widget
```

### 5. Consistent page sizing

Every page widget must have the same fixed dimensions. Calculate based on available viewer width:

```rust
fn calculate_page_dimensions(available_width: i32) -> (i32, i32) {
    let page_width = available_width - 32;  // 16px padding each side
    let page_height = (page_width as f64 * (1872.0 / 1404.0)) as i32;
    (page_width, page_height)
}
```

On window resize, recalculate dimensions and re-render visible pages at the new size (or let `GtkPicture` handle scaling with `ContentFit::Contain`).

### 6. Page number indicator

Update the page info label based on scroll position:

```
Page 3 of 12
```

The page number updates continuously as the user scrolls, based on which page's center is closest to the viewport center.

### 7. Visual page separators

Between pages, draw a thin separator with page numbers:

```
─────────── Page 3 ───────────
```

Use `GtkSeparator` with a `GtkLabel` overlay, or a custom drawn separator.

### 8. Scroll-to-page from sidebar

If the sidebar's page counter or navigation is used, smoothly scroll to the target page:

```rust
pub fn scroll_to_page(&self, page_number: usize) {
    // Calculate y offset for the target page
    let y = page_number * (page_height + separator_height + spacing);
    // Animate scroll
    let adj = self.scroll_window.vadjustment();
    // Use gtk animation or direct set
    adj.set_value(y as f64);
}
```

## Files to Create/Modify

- `src/ui/viewer.rs` — refactor to multi-page scroll mode
- `src/ui/mod.rs` — no changes needed

## Test Strategy

1. **Single-page document** — loads and displays one page, no scroll needed.
2. **Multi-page document (5 pages)** — all 5 page placeholders appear, scrollbar is visible.
3. **Lazy rendering** — load a 10-page doc, verify only pages 0–2 are rendered initially.
4. **Scroll triggers rendering** — scroll to page 5, verify pages 4–6 are rendered.
5. **Memory release** — scroll from page 1 to page 10, verify early pages release textures.
6. **Consistent sizing** — verify all page widgets have identical dimensions.
7. **Page counter** — scroll to different positions, verify page counter updates.
8. **Scroll-to-page** — call `scroll_to_page(5)`, verify viewport centers on page 5.
9. **Window resize** — resize window, verify pages adapt to new width.

## Acceptance Criteria

1. All pages of a notebook render in a continuous scrollable view.
2. Long documents (50+ pages) scroll smoothly thanks to lazy rendering.
3. Only visible pages (±1 buffer) consume memory.
4. Text and stroke sizes are consistent across all pages and all documents.
5. Page counter updates as the user scrolls.
6. Background rendering doesn't block the UI.
7. Rendered SVGs are cached to disk.
