//! Report what the parser recovers from `.rm` files, for diagnosing pages
//! that render blank or fail.
//!
//! Usage: `cargo run --example dump_rm -- <file.rm>...`

use rmsync::remarkable::rm_parser::parse_rm_from_path;
use rmsync::remarkable::svg_renderer::render_page_to_svg;
use std::path::Path;

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: dump_rm <file.rm>...");
        std::process::exit(2);
    }

    for arg in files {
        let path = Path::new(&arg);
        match parse_rm_from_path(path) {
            Ok(page) => {
                let (min_x, min_y, max_x, max_y) = page.bounding_box();
                let svg = render_page_to_svg(&page);
                println!("{}", path.display());
                println!(
                    "   version={} layers={} strokes={} points={}",
                    page.version,
                    page.layers.len(),
                    page.total_strokes(),
                    page.total_points()
                );
                println!("   bbox x {min_x:.1}..{max_x:.1}  y {min_y:.1}..{max_y:.1}");
                println!("   svg {} bytes", svg.len());
            }
            Err(e) => println!("{}: FAILED: {e}", path.display()),
        }
    }
}
