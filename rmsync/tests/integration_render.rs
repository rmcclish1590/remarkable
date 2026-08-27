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
