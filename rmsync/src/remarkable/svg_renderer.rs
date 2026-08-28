//! Convert parsed `RmPage` stroke data into SVG documents.
//!
//! The viewBox is computed from the actual bounding box of all strokes
//! (with padding) so content that extends beyond the reMarkable's default
//! 1404×1872 viewport — or uses negative coordinates from panning — is
//! always fully visible. Eraser strokes are dropped.

use crate::remarkable::rm_parser::{
    PenColor, PenType, RmPage, RmStroke, RmText, RmTextSpan, TextStyle,
};
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Version of the rendered-SVG output format. The viewer stamps its cache
/// directory with this and clears cached SVGs on mismatch, so bump it
/// whenever a parser or renderer change alters what pages look like —
/// otherwise fixes never reach pages that were already cached.
pub const RENDER_VERSION: u32 = 2;

const DEFAULT_W: f32 = 1404.0;
const DEFAULT_H: f32 = 1872.0;
const HIGHLIGHTER_OPACITY: f32 = 0.3;
const PADDING: f32 = 20.0;

/// Typed-text layout approximation. The tablet renders text with its own
/// typography; we approximate with generic families and an average glyph
/// width so pages are readable, not typographically identical.
const TEXT_LINE_HEIGHT: f32 = 1.4;
const TEXT_GLYPH_WIDTH: f32 = 0.5; // fraction of font size, for wrapping
const TEXT_SIZE_PLAIN: f32 = 32.0;
const TEXT_SIZE_HEADING: f32 = 48.0;

pub fn render_page_to_svg(page: &RmPage) -> String {
    let text_layout = page.text.as_ref().map(layout_text);

    // Use the bounding box of all content (padded), falling back to the
    // default reMarkable viewport for blank pages.
    let mut bounds: Option<(f32, f32, f32, f32)> = if page.total_strokes() > 0 {
        Some(page.bounding_box())
    } else {
        None
    };
    if let Some(layout) = &text_layout {
        bounds = Some(match bounds {
            Some((ax0, ay0, ax1, ay1)) => {
                let (bx0, by0, bx1, by1) = layout.extent;
                (ax0.min(bx0), ay0.min(by0), ax1.max(bx1), ay1.max(by1))
            }
            None => layout.extent,
        });
    }

    // The viewport is the union of the device page (0,0)–(1404,1872) and
    // the padded content box, so content fitting the page keeps natural
    // margins while oversized content stays fully visible on every edge.
    // (Clamping only the origin and reusing the padded width/height, as an
    // earlier version did, silently clipped the bottom of any page whose
    // content starts below the top margin and runs past the page height.)
    let (vb_x, vb_y, vb_w, vb_h) = if let Some((min_x, min_y, max_x, max_y)) = bounds {
        let x0 = (min_x - PADDING).min(0.0);
        let y0 = (min_y - PADDING).min(0.0);
        let x1 = (max_x + PADDING).max(DEFAULT_W);
        let y1 = (max_y + PADDING).max(DEFAULT_H);
        (x0, y0, x1 - x0, y1 - y0)
    } else {
        (0.0, 0.0, DEFAULT_W, DEFAULT_H)
    };

    let mut out = String::with_capacity(2048);
    write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb_x:.1} {vb_y:.1} {vb_w:.1} {vb_h:.1}" width="{w}" height="{h}">"#,
        w = vb_w as u32,
        h = vb_h as u32,
    )
    .unwrap();
    write!(
        out,
        r#"<rect x="{vb_x:.1}" y="{vb_y:.1}" width="{vb_w:.1}" height="{vb_h:.1}" fill="white"/>"#
    )
    .unwrap();

    for (i, layer) in page.layers.iter().enumerate() {
        write!(out, r#"<g id="layer-{i}">"#).unwrap();
        for stroke in &layer.strokes {
            render_stroke(&mut out, stroke);
        }
        out.push_str("</g>");
    }
    if let Some(layout) = &text_layout {
        render_text(&mut out, layout);
    }
    out.push_str("</svg>");
    out
}

/// A wrapped line of typed text, ready to emit.
struct TextLine {
    x: f32,
    baseline: f32,
    size: f32,
    /// Whole-paragraph bold (Heading and Bold paragraph styles).
    bold: bool,
    spans: Vec<RmTextSpan>,
}

struct TextLayout {
    lines: Vec<TextLine>,
    /// (min_x, min_y, max_x, max_y) the text occupies.
    extent: (f32, f32, f32, f32),
}

fn style_metrics(style: TextStyle) -> (f32, bool, Option<&'static str>, f32) {
    // (font size, paragraph bold, list prefix, indent)
    match style {
        TextStyle::Plain => (TEXT_SIZE_PLAIN, false, None, 0.0),
        TextStyle::Heading => (TEXT_SIZE_HEADING, true, None, 0.0),
        TextStyle::Bold => (TEXT_SIZE_PLAIN, true, None, 0.0),
        TextStyle::Bullet => (TEXT_SIZE_PLAIN, false, Some("\u{2022} "), 0.0),
        TextStyle::Bullet2 => (TEXT_SIZE_PLAIN, false, Some("\u{2022} "), TEXT_SIZE_PLAIN),
        TextStyle::Checkbox => (TEXT_SIZE_PLAIN, false, Some("\u{2610} "), 0.0),
        TextStyle::CheckboxChecked => (TEXT_SIZE_PLAIN, false, Some("\u{2611} "), 0.0),
    }
}

/// Wrap paragraphs into lines. Wrapping is an estimate from an average
/// glyph width — the goal is that long paragraphs stay on the page, not
/// that line breaks land exactly where the tablet puts them.
fn layout_text(text: &RmText) -> TextLayout {
    let mut lines = Vec::new();
    let mut cursor = text.pos_y as f32;
    for paragraph in &text.paragraphs {
        let (size, bold, prefix, indent) = style_metrics(paragraph.style);
        let advance = size * TEXT_LINE_HEIGHT;
        let budget =
            (((text.width - indent) / (size * TEXT_GLYPH_WIDTH)) as usize).max(8);

        let mut spans: Vec<RmTextSpan> = Vec::new();
        if let Some(prefix) = prefix {
            spans.push(RmTextSpan {
                text: prefix.to_string(),
                bold: false,
                italic: false,
            });
        }
        spans.extend(paragraph.spans.iter().cloned());

        let wrapped = wrap_spans(&spans, budget);
        if wrapped.is_empty() {
            // An empty paragraph is a blank line.
            cursor += advance;
            continue;
        }
        for line_spans in wrapped {
            cursor += advance;
            lines.push(TextLine {
                x: text.pos_x as f32 + indent,
                baseline: cursor - size * 0.3,
                size,
                bold,
                spans: line_spans,
            });
        }
    }

    let min_x = text.pos_x as f32;
    let min_y = text.pos_y as f32;
    let max_x = min_x + text.width.max(1.0);
    let max_y = cursor.max(min_y + 1.0);
    TextLayout {
        lines,
        extent: (min_x, min_y, max_x, max_y),
    }
}

/// Greedy word wrap over formatted spans, breaking at spaces where
/// possible and hard-breaking words longer than a whole line.
fn wrap_spans(spans: &[RmTextSpan], budget: usize) -> Vec<Vec<RmTextSpan>> {
    // Split into (word, formatting) tokens; a space token marks each gap.
    let mut tokens: Vec<(String, bool, bool)> = Vec::new();
    for span in spans {
        for piece in span.text.split_inclusive(' ') {
            let word = piece.trim_end_matches(' ');
            if !word.is_empty() {
                tokens.push((word.to_string(), span.bold, span.italic));
            }
            if piece.ends_with(' ') {
                tokens.push((" ".to_string(), span.bold, span.italic));
            }
        }
    }

    let mut lines: Vec<Vec<RmTextSpan>> = Vec::new();
    let mut current: Vec<RmTextSpan> = Vec::new();
    let mut current_len = 0usize;

    let push = |lines: &mut Vec<Vec<RmTextSpan>>,
                    current: &mut Vec<RmTextSpan>,
                    current_len: &mut usize| {
        if !current.is_empty() {
            lines.push(std::mem::take(current));
        }
        *current_len = 0;
    };

    for (word, bold, italic) in tokens {
        let is_space = word == " ";
        let mut word = word;
        if is_space && current_len == 0 {
            continue; // never start a line with the wrap gap
        }
        let mut len = word.chars().count();
        while !is_space && current_len + len > budget {
            if current_len == 0 {
                // A single word longer than the line: hard-break it.
                let head: String = word.chars().take(budget).collect();
                let tail: String = word.chars().skip(budget).collect();
                append_span(&mut current, head, bold, italic);
                push(&mut lines, &mut current, &mut current_len);
                word = tail;
                len = word.chars().count();
                if len == 0 {
                    break;
                }
            } else {
                push(&mut lines, &mut current, &mut current_len);
            }
        }
        if word.is_empty() || (is_space && current_len == 0) {
            continue;
        }
        append_span(&mut current, word.clone(), bold, italic);
        current_len += len;
    }
    if !current.is_empty() {
        // Trailing spaces don't earn a line of their own.
        if current.iter().any(|s| !s.text.trim().is_empty()) {
            lines.push(current);
        }
    }
    lines
}

fn append_span(line: &mut Vec<RmTextSpan>, text: String, bold: bool, italic: bool) {
    match line.last_mut() {
        Some(s) if s.bold == bold && s.italic == italic => s.text.push_str(&text),
        _ => line.push(RmTextSpan { text, bold, italic }),
    }
}

fn render_text(out: &mut String, layout: &TextLayout) {
    if layout.lines.is_empty() {
        return;
    }
    out.push_str(r#"<g id="text">"#);
    for line in &layout.lines {
        write!(
            out,
            r##"<text x="{:.2}" y="{:.2}" font-family="sans-serif" font-size="{:.1}" fill="#000000""##,
            line.x, line.baseline, line.size
        )
        .unwrap();
        if line.bold {
            out.push_str(r#" font-weight="bold""#);
        }
        out.push('>');
        for span in &line.spans {
            let needs_tspan = (span.bold && !line.bold) || span.italic;
            if needs_tspan {
                out.push_str("<tspan");
                if span.bold && !line.bold {
                    out.push_str(r#" font-weight="bold""#);
                }
                if span.italic {
                    out.push_str(r#" font-style="italic""#);
                }
                out.push('>');
                out.push_str(&xml_escape(&span.text));
                out.push_str("</tspan>");
            } else {
                out.push_str(&xml_escape(&span.text));
            }
        }
        out.push_str("</text>");
    }
    out.push_str("</g>");
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn render_page_to_svg_file(page: &RmPage, output_path: &Path) -> Result<()> {
    let svg = render_page_to_svg(page);
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory {}", parent.display()))?;
        }
    }
    fs::write(output_path, svg)
        .with_context(|| format!("writing SVG to {}", output_path.display()))?;
    Ok(())
}

pub fn render_document_pages(
    pages: &[RmPage],
    output_dir: &Path,
    doc_uuid: &str,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output directory {}", output_dir.display()))?;
    let mut paths = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let path = output_dir.join(format!("{doc_uuid}-page-{i:04}.svg"));
        render_page_to_svg_file(page, &path)?;
        paths.push(path);
    }
    Ok(paths)
}

fn render_stroke(out: &mut String, stroke: &RmStroke) {
    if matches!(stroke.pen, PenType::Eraser | PenType::EraseArea) {
        return;
    }
    if stroke.points.is_empty() {
        return;
    }
    let color = pen_color_to_svg(&stroke.color);
    match stroke.pen {
        PenType::Highlighter => render_polyline(out, stroke, color, true),
        PenType::Brush
        | PenType::TiltPencil
        | PenType::SharpPencil
        | PenType::CalligraphyPen => render_variable_width(out, stroke, color),
        _ => render_polyline(out, stroke, color, false),
    }
}

fn render_polyline(out: &mut String, stroke: &RmStroke, color: &str, highlighter: bool) {
    let mut points = String::with_capacity(stroke.points.len() * 16);
    for (i, p) in stroke.points.iter().enumerate() {
        if i > 0 {
            points.push(' ');
        }
        write!(points, "{:.2},{:.2}", p.x, p.y).unwrap();
    }
    if highlighter {
        write!(
            out,
            r#"<polyline points="{points}" stroke="{color}" stroke-width="{:.3}" fill="none" stroke-linecap="square" opacity="{:.2}"/>"#,
            stroke.base_width, HIGHLIGHTER_OPACITY
        )
        .unwrap();
    } else {
        write!(
            out,
            r#"<polyline points="{points}" stroke="{color}" stroke-width="{:.3}" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
            stroke.base_width
        )
        .unwrap();
    }
}

fn render_variable_width(out: &mut String, stroke: &RmStroke, color: &str) {
    if stroke.points.len() < 2 {
        return;
    }
    for w in stroke.points.windows(2) {
        let a = &w[0];
        let b = &w[1];
        let avg = (a.width + b.width) / 2.0;
        let stroke_w = if avg > 0.0 { avg } else { stroke.base_width };
        write!(
            out,
            r#"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="{color}" stroke-width="{:.3}" stroke-linecap="round"/>"#,
            a.x, a.y, b.x, b.y, stroke_w
        )
        .unwrap();
    }
}

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
        PenColor::Unknown(_) => "#000000",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remarkable::rm_parser::{RmLayer, RmParagraph, RmPoint, RmStroke};
    use tempfile::tempdir;

    fn pt(x: f32, y: f32) -> RmPoint {
        RmPoint {
            x,
            y,
            speed: 0.0,
            direction: 0.0,
            width: 2.0,
            pressure: 0.5,
        }
    }

    fn page_with(strokes_per_layer: Vec<Vec<RmStroke>>) -> RmPage {
        RmPage {
            version: 6,
            layers: strokes_per_layer
                .into_iter()
                .map(|strokes| RmLayer { strokes })
                .collect(),
            text: None,
        }
    }

    fn stroke(pen: PenType, color: PenColor, points: Vec<RmPoint>) -> RmStroke {
        RmStroke {
            pen,
            color,
            base_width: 2.0,
            points,
        }
    }

    #[test]
    fn empty_page_renders_background_only() {
        let page = page_with(vec![]);
        let svg = render_page_to_svg(&page);
        // Empty page falls back to the default 1404×1872 viewport.
        assert!(svg.contains("viewBox=\"0.0 0.0 1404.0 1872.0\""));
        assert!(svg.contains("fill=\"white\""));
        assert!(!svg.contains("<g "));
        assert!(!svg.contains("<polyline"));
        assert!(svg.ends_with("</svg>"));
    }

    #[test]
    fn single_stroke_emits_polyline_with_points() {
        let page = page_with(vec![vec![stroke(
            PenType::Fineliner,
            PenColor::Black,
            vec![pt(10.0, 20.0), pt(30.0, 40.0), pt(50.0, 60.0)],
        )]]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains(r#"<g id="layer-0">"#));
        assert!(svg.contains("<polyline"));
        assert!(svg.contains(r#"points="10.00,20.00 30.00,40.00 50.00,60.00""#));
        assert!(svg.contains(r##"stroke="#000000""##));
        assert!(svg.contains(r#"fill="none""#));
        assert!(svg.contains(r#"stroke-linecap="round""#));
    }

    #[test]
    fn highlighter_emits_opacity_and_square_caps() {
        let page = page_with(vec![vec![stroke(
            PenType::Highlighter,
            PenColor::Yellow,
            vec![pt(0.0, 0.0), pt(100.0, 0.0)],
        )]]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains(r#"opacity="0.30""#));
        assert!(svg.contains(r#"stroke-linecap="square""#));
        assert!(svg.contains(r##"stroke="#FFEB3B""##));
    }

    #[test]
    fn variable_width_pen_emits_line_segments() {
        let mut p1 = pt(0.0, 0.0);
        let mut p2 = pt(10.0, 0.0);
        let mut p3 = pt(20.0, 0.0);
        p1.width = 1.0;
        p2.width = 3.0;
        p3.width = 5.0;
        let page = page_with(vec![vec![stroke(
            PenType::Brush,
            PenColor::Blue,
            vec![p1, p2, p3],
        )]]);
        let svg = render_page_to_svg(&page);
        assert!(!svg.contains("<polyline"));
        let segments = svg.matches("<line").count();
        assert_eq!(segments, 2);
        // First segment averages 1.0 and 3.0 → 2.0
        assert!(svg.contains(r#"stroke-width="2.000""#));
        // Second averages 3.0 and 5.0 → 4.0
        assert!(svg.contains(r#"stroke-width="4.000""#));
    }

    #[test]
    fn eraser_strokes_are_omitted() {
        let page = page_with(vec![vec![
            stroke(
                PenType::Eraser,
                PenColor::Black,
                vec![pt(0.0, 0.0), pt(10.0, 10.0)],
            ),
            stroke(
                PenType::EraseArea,
                PenColor::Black,
                vec![pt(0.0, 0.0), pt(10.0, 10.0)],
            ),
        ]]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains(r#"<g id="layer-0">"#));
        assert!(!svg.contains("<polyline"));
        assert!(!svg.contains("<line"));
    }

    #[test]
    fn all_colors_map_to_hex() {
        for (color, hex) in [
            (PenColor::Black, "#000000"),
            (PenColor::Grey, "#808080"),
            (PenColor::White, "#FFFFFF"),
            (PenColor::Yellow, "#FFEB3B"),
            (PenColor::Green, "#4CAF50"),
            (PenColor::Pink, "#E91E63"),
            (PenColor::Blue, "#2196F3"),
            (PenColor::Red, "#F44336"),
            (PenColor::GrayOverlap, "#A0A0A0"),
            (PenColor::Unknown(99), "#000000"),
        ] {
            let page = page_with(vec![vec![stroke(
                PenType::Fineliner,
                color,
                vec![pt(0.0, 0.0), pt(1.0, 1.0)],
            )]]);
            let svg = render_page_to_svg(&page);
            assert!(
                svg.contains(&format!(r#"stroke="{hex}""#)),
                "color {color:?} did not render as {hex}: {svg}"
            );
        }
    }

    #[test]
    fn multiple_layers_in_z_order() {
        let page = page_with(vec![
            vec![stroke(
                PenType::Fineliner,
                PenColor::Black,
                vec![pt(0.0, 0.0), pt(1.0, 1.0)],
            )],
            vec![stroke(
                PenType::Fineliner,
                PenColor::Red,
                vec![pt(2.0, 2.0), pt(3.0, 3.0)],
            )],
        ]);
        let svg = render_page_to_svg(&page);
        let l0 = svg.find(r#"<g id="layer-0">"#).expect("layer-0 group");
        let l1 = svg.find(r#"<g id="layer-1">"#).expect("layer-1 group");
        assert!(l0 < l1, "layer-0 must appear before layer-1");
    }

    fn text_page(paragraphs: Vec<RmParagraph>) -> RmPage {
        RmPage {
            version: 6,
            layers: vec![],
            text: Some(RmText {
                paragraphs,
                pos_x: -468.0,
                pos_y: 234.0,
                width: 936.0,
            }),
        }
    }

    fn span(text: &str) -> RmTextSpan {
        RmTextSpan {
            text: text.to_string(),
            bold: false,
            italic: false,
        }
    }

    #[test]
    fn typed_text_renders_as_text_elements() {
        let page = text_page(vec![
            RmParagraph {
                style: TextStyle::Heading,
                spans: vec![span("My Notes")],
            },
            RmParagraph {
                style: TextStyle::Plain,
                spans: vec![span("hello world")],
            },
        ]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains(r#"<g id="text">"#));
        assert!(svg.contains("My Notes"));
        assert!(svg.contains("hello world"));
        // Headings are larger and bold.
        assert!(svg.contains(r##"font-size="48.0" fill="#000000" font-weight="bold""##));
        // The viewport must cover the text block, not collapse to 0×0.
        assert!(!svg.contains(r#"viewBox="0.0 0.0 0.0 0.0""#));
    }

    #[test]
    fn text_is_xml_escaped() {
        let page = text_page(vec![RmParagraph {
            style: TextStyle::Plain,
            spans: vec![span("a<b & c>\"d\"")],
        }]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains("a&lt;b &amp; c&gt;&quot;d&quot;"));
        assert!(!svg.contains("a<b"));
    }

    #[test]
    fn long_paragraph_wraps_into_multiple_lines() {
        let long = "word ".repeat(100);
        let page = text_page(vec![RmParagraph {
            style: TextStyle::Plain,
            spans: vec![span(&long)],
        }]);
        let svg = render_page_to_svg(&page);
        let lines = svg.matches("<text ").count();
        assert!(lines > 5, "500 chars at ~58 chars/line: got {lines} lines");
    }

    #[test]
    fn bullet_paragraphs_get_a_marker_prefix() {
        let page = text_page(vec![RmParagraph {
            style: TextStyle::Bullet,
            spans: vec![span("item")],
        }]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains("\u{2022} item"));
    }

    #[test]
    fn inline_bold_and_italic_become_tspans() {
        let page = text_page(vec![RmParagraph {
            style: TextStyle::Plain,
            spans: vec![
                span("plain "),
                RmTextSpan {
                    text: "bold".to_string(),
                    bold: true,
                    italic: false,
                },
                RmTextSpan {
                    text: " ital".to_string(),
                    bold: false,
                    italic: true,
                },
            ],
        }]);
        let svg = render_page_to_svg(&page);
        assert!(svg.contains(r#"<tspan font-weight="bold">bold</tspan>"#));
        assert!(svg.contains(r#"<tspan font-style="italic"> ital</tspan>"#));
    }

    #[test]
    fn text_and_strokes_render_together() {
        let mut page = page_with(vec![vec![stroke(
            PenType::Fineliner,
            PenColor::Black,
            vec![pt(10.0, 20.0), pt(30.0, 40.0)],
        )]]);
        page.text = Some(RmText {
            paragraphs: vec![RmParagraph {
                style: TextStyle::Plain,
                spans: vec![span("annotation")],
            }],
            pos_x: -468.0,
            pos_y: 234.0,
            width: 936.0,
        });
        let svg = render_page_to_svg(&page);
        assert!(svg.contains("<polyline"));
        assert!(svg.contains("annotation"));
        // viewBox must cover the text block's left edge (x = -468).
        assert!(svg.contains(r#"viewBox="-488.0"#), "svg was: {}", &svg[..120]);
    }

    #[test]
    fn tall_content_starting_below_top_margin_is_not_bottom_clipped() {
        // Regression: content from y=234 down to y=4000 used to get a
        // viewBox whose bottom sat above the lowest content because the
        // origin was clamped to 0 without re-extending the height.
        let page = page_with(vec![vec![stroke(
            PenType::Fineliner,
            PenColor::Black,
            vec![pt(100.0, 234.0), pt(100.0, 4000.0)],
        )]]);
        let svg = render_page_to_svg(&page);
        // Bottom edge must reach max_y + padding: 0 → 4020 on the y axis.
        assert!(
            svg.contains(r#"viewBox="0.0 0.0 1404.0 4020.0""#),
            "svg was: {}",
            &svg[..130]
        );
    }

    #[test]
    fn wrap_spans_hard_breaks_oversized_words() {
        let lines = wrap_spans(&[span(&"x".repeat(25))], 10);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0][0].text.len(), 10);
        assert_eq!(lines[2][0].text.len(), 5);
    }

    #[test]
    fn writes_file_and_creates_parent_dirs() {
        let td = tempdir().unwrap();
        let path = td.path().join("nested/sub/page.svg");
        let page = page_with(vec![vec![stroke(
            PenType::Fineliner,
            PenColor::Black,
            vec![pt(1.0, 2.0), pt(3.0, 4.0)],
        )]]);
        render_page_to_svg_file(&page, &path).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("<svg "));
        assert!(written.contains("viewBox=\""));
        assert!(written.contains("fill=\"white\""));
        assert!(written.ends_with("</svg>"));
    }

    #[test]
    fn document_pages_writes_one_file_per_page() {
        let td = tempdir().unwrap();
        let pages = vec![page_with(vec![]), page_with(vec![]), page_with(vec![])];
        let paths = render_document_pages(&pages, td.path(), "abc-uuid").unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[0].ends_with("abc-uuid-page-0000.svg"));
        assert!(paths[1].ends_with("abc-uuid-page-0001.svg"));
        assert!(paths[2].ends_with("abc-uuid-page-0002.svg"));
        for p in &paths {
            assert!(p.exists(), "expected {} to exist", p.display());
        }
    }
}
