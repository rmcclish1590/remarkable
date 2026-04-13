//! Convert parsed `RmPage` stroke data into SVG documents.
//!
//! All output uses a fixed 1404×1872 viewport (the reMarkable's native
//! resolution) so pages render at consistent size regardless of stroke
//! density. Eraser strokes are dropped — their effect is already baked
//! into the surviving stroke set on the device.

use crate::remarkable::rm_parser::{PenColor, PenType, RmPage, RmStroke};
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const VIEW_W: u32 = 1404;
const VIEW_H: u32 = 1872;
const HIGHLIGHTER_OPACITY: f32 = 0.3;

pub fn render_page_to_svg(page: &RmPage) -> String {
    let mut out = String::with_capacity(2048);
    write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {VIEW_W} {VIEW_H}" width="{VIEW_W}" height="{VIEW_H}">"#
    )
    .unwrap();
    write!(
        out,
        r#"<rect width="{VIEW_W}" height="{VIEW_H}" fill="white"/>"#
    )
    .unwrap();

    for (i, layer) in page.layers.iter().enumerate() {
        write!(out, r#"<g id="layer-{i}">"#).unwrap();
        for stroke in &layer.strokes {
            render_stroke(&mut out, stroke);
        }
        out.push_str("</g>");
    }
    out.push_str("</svg>");
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
    use crate::remarkable::rm_parser::{RmLayer, RmPoint, RmStroke};
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
        assert!(svg.contains(r#"viewBox="0 0 1404 1872""#));
        assert!(svg.contains(r#"<rect width="1404" height="1872" fill="white"/>"#));
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
        assert!(written.contains(r#"viewBox="0 0 1404 1872""#));
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
