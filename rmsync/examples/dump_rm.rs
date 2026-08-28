//! Report what the parser recovers from `.rm` files, for diagnosing pages
//! that render blank or fail.
//!
//! Usage: `cargo run --example dump_rm -- [--svg-out <dir>] <file.rm>...`
//!
//! With `--svg-out`, the rendered SVG for each page is also written into
//! the given directory (named after the input file), so the output can be
//! inspected in a browser.

use rmsync::remarkable::rm_parser::parse_rm_from_path;
use rmsync::remarkable::svg_renderer::render_page_to_svg;
use std::path::{Path, PathBuf};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut svg_out: Option<PathBuf> = None;
    if args.first().map(String::as_str) == Some("--svg-out") {
        if args.len() < 2 {
            eprintln!("--svg-out requires a directory argument");
            std::process::exit(2);
        }
        svg_out = Some(PathBuf::from(args.remove(1)));
        args.remove(0);
    }
    let files = args;
    if files.is_empty() {
        eprintln!("usage: dump_rm [--svg-out <dir>] <file.rm>...");
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
                if let Some(text) = &page.text {
                    println!(
                        "   text {} paragraphs, {} chars, pos=({:.1}, {:.1}) width={:.1}",
                        text.paragraphs.len(),
                        text.char_count(),
                        text.pos_x,
                        text.pos_y,
                        text.width
                    );
                }
                println!("   svg {} bytes", svg.len());
                if let Some(dir) = &svg_out {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                    let out = dir.join(format!("{stem}.svg"));
                    if let Err(e) =
                        std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&out, &svg))
                    {
                        eprintln!("   failed to write {}: {e}", out.display());
                    } else {
                        println!("   wrote {}", out.display());
                    }
                }
            }
            Err(e) => println!("{}: FAILED: {e}", path.display()),
        }
    }
}
