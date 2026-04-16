//! Shared integration-test helpers.
//!
//! These helpers deliberately use only the parts of `rmsync` that don't
//! require an SSH/SFTP connection: parsers, renderers, scanners,
//! state_db, and the diff engine. A full mock for
//! `DeviceConnection` would require a trait-based refactor of the
//! transfer layer and is out of scope for this test harness.

// Each integration-test binary compiles `common` independently; helpers
// used by one binary but not another would otherwise show up as dead
// code. Silence those to keep the module genuinely shared.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// Produce a minimal valid reMarkable .rm v6 file with a single layer
/// containing no strokes — enough to exercise the parser + SVG renderer
/// end-to-end.
pub fn create_minimal_rm_v6(output: &Path) -> std::io::Result<()> {
    // Matches rmsync::remarkable::rm_parser expectations:
    // 43-byte header padded with spaces, 10-byte post-header pad, i32 LE
    // layer count (0 = no layers).
    const HEADER_LEN: usize = 43;
    const POST_HEADER_PAD: usize = 10;
    let prefix = b"reMarkable .lines file, version=6";
    let mut data = Vec::with_capacity(HEADER_LEN + POST_HEADER_PAD + 4);
    data.extend_from_slice(prefix);
    data.resize(HEADER_LEN, b' ');
    data.extend_from_slice(&[0u8; POST_HEADER_PAD]);
    data.extend_from_slice(&0_i32.to_le_bytes());
    fs::write(output, &data)
}

/// Write a minimal `.metadata` JSON for a document.
pub fn write_metadata(
    dir: &Path,
    uuid: &str,
    parent: &str,
    doc_type: &str,
    name: &str,
    deleted: bool,
) {
    let body = format!(
        r#"{{
          "deleted": {deleted},
          "lastModified": "1",
          "parent": "{parent}",
          "pinned": false,
          "type": "{doc_type}",
          "visibleName": "{name}"
        }}"#
    );
    fs::write(dir.join(format!("{uuid}.metadata")), body).unwrap();
}

/// Write a minimal `.content` JSON with the given list of page UUIDs.
pub fn write_content(dir: &Path, uuid: &str, page_ids: &[&str]) {
    let pages: Vec<String> = page_ids.iter().map(|p| format!("\"{p}\"")).collect();
    let body = format!(
        r#"{{
          "fileType": "notebook",
          "formatVersion": 2,
          "pageCount": {pc},
          "pages": [{joined}]
        }}"#,
        pc = page_ids.len(),
        joined = pages.join(",")
    );
    fs::write(dir.join(format!("{uuid}.content")), body).unwrap();
}

/// Build a whole notebook: metadata + content + UUID/page1.rm.
pub fn seed_notebook(raw: &Path, uuid: &str, name: &str, parent: &str) {
    write_metadata(raw, uuid, parent, "DocumentType", name, false);
    write_content(raw, uuid, &["page1"]);
    let sub = raw.join(uuid);
    fs::create_dir_all(&sub).unwrap();
    create_minimal_rm_v6(&sub.join("page1.rm")).unwrap();
}
