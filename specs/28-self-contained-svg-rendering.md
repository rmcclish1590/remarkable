# Spec 28 — Self-Contained SVG Rendering Pipeline

**Layer:** 5 — Viewer  
**Dependencies:** 26 (content v2 parser), 04 (SVG renderer)  
**Estimated effort:** 1–2 hours  
**Priority:** Medium — viewer currently depends on system librsvg at runtime  

## Objective

Replace the `GtkPicture::for_filename` SVG loading path (which silently fails when the system's gdk-pixbuf SVG loader isn't installed) with a self-contained pipeline that uses `resvg` (already a project dependency) to render SVG to a pixel buffer, then displays the result via `gdk::MemoryTexture`. This makes document viewing work on any Linux system without requiring librsvg as a runtime dependency.

## Context

The current viewer calls `gtk::Picture::for_filename(svg_path)` which delegates SVG decoding to `gdk-pixbuf`'s SVG loader, backed by `librsvg2-common`. If that package is missing:

- GTK defers file loading to paint-time.
- The pixbuf loader fails to find an SVG handler.
- `GtkPicture` silently renders a blank/transparent image.
- **No error is returned to application code.** No log output. No crash.

The user sees pages in the scroll area that are the correct height (the size-request is set) but completely blank. This is indistinguishable from "the renderer produced empty SVG" — impossible to diagnose without developer knowledge.

`resvg 0.44` is already in `Cargo.toml`. It can render any SVG to a pixel buffer in-process, eliminating the external dependency entirely.

## Technical Requirements

### 1. Render SVG → pixel buffer via resvg

```rust
fn svg_to_texture(svg_bytes: &[u8], scale: f32) -> Result<gdk::MemoryTexture> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg_bytes, &options)?;
    let size = tree.size();
    let width = (size.width() * scale) as u32;
    let height = (size.height() * scale) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let bytes = glib::Bytes::from(pixmap.data());
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (width * 4) as usize,
    );
    Ok(texture)
}
```

### 2. Replace `build_page_widget` in `src/ui/viewer.rs`

Instead of:
```rust
let picture = gtk::Picture::for_filename(svg_path);
```

Do:
```rust
let svg_bytes = std::fs::read(svg_path)?;
let texture = svg_to_texture(&svg_bytes, 1.0)?;
let picture = gtk::Picture::for_paintable(Some(&texture));
```

### 3. Dynamic scaling based on panel width

The reMarkable page viewport is 1404×1872 pixels. Rendering at full resolution for every page in a 100-page notebook would consume ~1 GB of memory.

Instead, render at the viewer panel's current width:

```rust
fn render_scale_for_width(available_width: i32) -> f32 {
    let padding = 32; // 16px margin each side
    let target = (available_width - padding).max(200) as f32;
    target / 1404.0
}
```

On a typical 600px-wide viewer pane, scale ≈ 0.4, so each page is ~600×800 px (~1.9 MB) — manageable.

### 4. Re-render on window resize

Connect to the viewer pane's `notify::width` signal. When the width changes significantly (>20px delta), re-render visible pages at the new scale. Use a debounce timer (200ms) to avoid thrashing during drag-resize.

### 5. Cache rendered textures

Keep the rendered `gdk::MemoryTexture` for each page in a `Vec<Option<gdk::MemoryTexture>>`. On scroll, load from cache or render from SVG. On resize, invalidate the cache.

### 6. Remove librsvg runtime dependency

Update `specs/25-deb-packaging.md` and `Cargo.toml` `[package.metadata.deb]`: remove `librsvg2-common` from the dependency list or documentation, since SVG rendering is now fully self-contained.

## Files to Create/Modify

- `src/ui/viewer.rs` — add `svg_to_texture`, replace `for_filename` with texture pipeline, add resize handling
- `Cargo.toml` — no changes needed (`resvg` already present)
- `specs/25-deb-packaging.md` — note that `librsvg2-common` is no longer required

## Test Strategy

1. **Render known SVG** — create a minimal valid SVG string, pass to `svg_to_texture`, verify the returned texture has correct dimensions.
2. **Scale factor** — render at scale 0.5 (702×936) and 1.0 (1404×1872), verify pixel dimensions match.
3. **Invalid SVG** — pass malformed bytes, verify error returned (not a panic or blank).
4. **Memory bound** — render 50 pages at scale 0.5, verify peak memory stays under 200 MB.
5. **Resize triggers re-render** — mock a width change, verify cached textures are invalidated and new ones produced.

## Acceptance Criteria

1. Documents render correctly without `librsvg2-common` installed.
2. Page images scale to the viewer panel width while preserving 1404:1872 aspect ratio.
3. Text and stroke sizes are consistent across all pages and all documents at any viewer width.
4. Resizing the window re-renders pages at the new width (debounced, not per-pixel).
5. Memory usage stays bounded — only visible pages (±1 buffer) hold pixel data.
6. All unit tests pass.
