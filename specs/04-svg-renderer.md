# Spec 04 — SVG Renderer (Strokes → SVG)

**Layer:** 0 — Foundation  
**Dependencies:** 03 (rm binary parser)  
**Estimated effort:** 1–2 hours  

## Objective

Convert parsed `RmPage` stroke data into SVG documents at the reMarkable's native resolution, producing output that visually matches what the tablet displays.

## Context

The .rm parser (Spec 03) produces `RmPage` structs containing layers of strokes with point data. This spec converts those structs into SVG strings/files. The SVG will be displayed in the GTK4 viewer (Spec 20/21) and cached to disk. All SVGs must use a consistent 1404×1872 viewport so text and strokes appear at uniform size across all documents.

## Technical Requirements

### 1. SVG generation (`src/remarkable/svg_renderer.rs`)

```rust
/// Render a parsed .rm page to an SVG string.
pub fn render_page_to_svg(page: &RmPage) -> String

/// Render a page and write the SVG to a file.
pub fn render_page_to_svg_file(page: &RmPage, output_path: &Path) -> Result<()>

/// Render all pages of a document to individual SVG files.
/// Returns the list of output paths in page order.
pub fn render_document_pages(
    pages: &[RmPage],
    output_dir: &Path,
    doc_uuid: &str,
) -> Result<Vec<PathBuf>>
```

### 2. SVG document structure

Every SVG must use this consistent viewport:

```xml
<svg xmlns="http://www.w3.org/2000/svg"
     viewBox="0 0 1404 1872"
     width="1404"
     height="1872">
  <!-- Background (white) -->
  <rect width="1404" height="1872" fill="white"/>
  
  <!-- Layer 1 -->
  <g id="layer-0">
    <!-- Strokes as <polyline> or <path> elements -->
  </g>
  
  <!-- Layer 2 -->
  <g id="layer-1">
    ...
  </g>
</svg>
```

### 3. Stroke rendering rules

Each stroke becomes either a `<polyline>` (for uniform-width strokes) or a `<path>` (for variable-width strokes):

**Uniform-width pens** (Fineliner, BallPoint with constant pressure):
```xml
<polyline points="x1,y1 x2,y2 x3,y3 ..."
          stroke="{color}" stroke-width="{width}"
          fill="none" stroke-linecap="round" stroke-linejoin="round"/>
```

**Variable-width pens** (Brush, Pencil, CalligraphyPen — where point `width` varies):
For each segment between two points, draw a line with the average width of the two endpoints:
```xml
<line x1="{p1.x}" y1="{p1.y}" x2="{p2.x}" y2="{p2.y}"
      stroke="{color}" stroke-width="{avg_width}"
      stroke-linecap="round"/>
```

Or, for smoother results, use a `<path>` with varying stroke widths via individual segments.

**Highlighter**: Same as above but with `opacity="0.3"` and blending:
```xml
<polyline ... stroke="{color}" stroke-width="{width}"
          opacity="0.3" stroke-linecap="square"/>
```

**Eraser / EraseArea**: Skip these strokes entirely in SVG output. (They modify the canvas on-device but the net result is captured in the stroke data that remains.)

### 4. Color mapping

```rust
fn pen_color_to_svg(color: &PenColor) -> &'static str {
    match color {
        PenColor::Black => "#000000",
        PenColor::Grey => "#808080",
        PenColor::White => "#FFFFFF",
        PenColor::Yellow => "#FFEB3B",
        PenColor::Green => "#4CAF50",
        PenColor::Pink => "#E91E63",
        PenColor::Blue => "#2196F3",
        PenColor::Red => "#F44336",
        PenColor::GrayOverlap => "#A0A0A0",
        PenColor::Unknown(_) => "#000000",  // Default to black
    }
}
```

### 5. Width scaling

The raw `base_width` from the .rm file needs scaling. The reMarkable uses these approximate base widths:
- Thin: ~1.875
- Medium: ~2.0  
- Thick: ~2.125

For variable-width pens, the per-point `width` field already contains the pressure-adjusted width. Use it directly. For fixed-width pens (Fineliner), use `base_width * 1.0` (no adjustment needed — the base_width is the display width).

### 6. Layer ordering

Render layers in order (layer 0 first, drawn at the back). Later layers are drawn on top. Use SVG `<g>` groups with `id="layer-{n}"` for each layer.

## Files to Create/Modify

- `src/remarkable/svg_renderer.rs` — full implementation
- `src/remarkable/mod.rs` — export the module

## Test Strategy

1. **Empty page** — render an `RmPage` with no layers. Output should be a valid SVG with only the white background rect.
2. **Single stroke** — create an `RmPage` with one layer, one stroke, 3 points. Verify the SVG contains a `<polyline>` with the correct coordinates.
3. **Color mapping** — render strokes of each color, verify SVG color attributes match.
4. **Highlighter opacity** — render a Highlighter stroke, verify `opacity="0.3"` is present.
5. **Multiple layers** — render 2 layers, verify they appear as separate `<g>` elements in correct order.
6. **File output** — use `render_page_to_svg_file`, verify the file is written and is valid XML.
7. **Viewport consistency** — every SVG output must contain `viewBox="0 0 1404 1872"`.

## Acceptance Criteria

1. `render_page_to_svg` produces valid SVG for any `RmPage` input.
2. Viewport is always 1404×1872 (consistent sizing requirement).
3. Colors, widths, and opacity map correctly per pen type.
4. Eraser strokes are excluded from SVG output.
5. Multi-layer pages render with correct z-ordering.
6. Output SVGs can be opened and viewed in a web browser or `eog` (Eye of GNOME).
7. All unit tests pass.
