//! End-to-end render pipeline tests: .rm binary → parsed page → SVG.

mod common;

use rmsync::remarkable::rm_parser::parse_rm_file;
use rmsync::remarkable::svg_renderer::render_page_to_svg;
use tempfile::tempdir;

#[test]
fn minimal_rm_file_parses_and_renders_to_valid_svg() {
    let dir = tempdir().unwrap();
    let rm_path = dir.path().join("p.rm");
    common::create_minimal_rm_v6(&rm_path).unwrap();

    let bytes = std::fs::read(&rm_path).unwrap();
    let page = parse_rm_file(&bytes).expect("parse succeeds");
    let svg = render_page_to_svg(&page);

    assert!(svg.starts_with("<svg"));
    assert!(svg.ends_with("</svg>"));
    // Empty page (0 layers) uses the default 1404×1872 viewport.
    assert!(svg.contains("viewBox=\"0.0 0.0 1404.0 1872.0\""));
}

/// Real tablets write v6 as a block/scene stream, not the older flat layout.
/// Regression guard for pages that parsed as empty and fell through to the
/// rmscene fallback (MCC-37).
#[test]
fn v6_block_file_parses_and_renders_its_strokes() {
    let dir = tempdir().unwrap();
    let rm_path = dir.path().join("p.rm");
    let points = [(100.0, 200.0), (150.0, 250.0), (200.0, 300.0)];
    common::create_rm_v6_with_stroke(&rm_path, &points).unwrap();

    let bytes = std::fs::read(&rm_path).unwrap();
    let page = parse_rm_file(&bytes).expect("v6 block file parses");

    assert_eq!(page.version, 6);
    assert_eq!(page.total_strokes(), 1, "the line block became a stroke");
    assert_eq!(page.total_points(), points.len());

    let svg = render_page_to_svg(&page);
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("<polyline"), "ballpoint renders as a polyline");
    for (x, y) in points {
        assert!(
            svg.contains(&format!("{x:.2},{y:.2}")),
            "point ({x}, {y}) reached the SVG"
        );
    }
}
