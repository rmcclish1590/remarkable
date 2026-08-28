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

const HEADER_LEN: usize = 43;
const BLOCK_SCENE_LINE_ITEM: u8 = 0x05;
const BLOCK_MIGRATION_INFO: u8 = 0x00;
const ITEM_TYPE_LINE: u8 = 0x03;
const TAG_ID: u8 = 0xF;
const TAG_LENGTH4: u8 = 0xC;
const TAG_BYTE8: u8 = 0x8;
const TAG_BYTE4: u8 = 0x4;

/// Produce a minimal valid reMarkable .rm v6 file that carries no strokes —
/// enough to exercise the parser + SVG renderer end-to-end.
pub fn create_minimal_rm_v6(output: &Path) -> std::io::Result<()> {
    let mut data = rm_v6_header();
    data.extend_from_slice(&v6_block(BLOCK_MIGRATION_INFO, 1, &[0u8; 7]));
    fs::write(output, &data)
}

/// Produce a v6 file holding one stroke through the given points — the
/// block/scene layout real tablets write, as opposed to the older flat one.
pub fn create_rm_v6_with_stroke(output: &Path, points: &[(f32, f32)]) -> std::io::Result<()> {
    let mut data = rm_v6_header();
    data.extend_from_slice(&v6_block(BLOCK_MIGRATION_INFO, 1, &[0u8; 7]));
    data.extend_from_slice(&v6_block(BLOCK_SCENE_LINE_ITEM, 2, &v6_line_payload(points)));
    fs::write(output, &data)
}

fn rm_v6_header() -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_LEN);
    data.extend_from_slice(b"reMarkable .lines file, version=6");
    data.resize(HEADER_LEN, b' ');
    data
}

/// Block header: u32 payload length, u8 unknown, u8 min version,
/// u8 current version, u8 block type — then the payload.
fn v6_block(block_type: u8, version: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0, 1, version, block_type]);
    out.extend_from_slice(payload);
    out
}

fn v6_varuint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

/// A tagged-value header: field index in the upper bits, type in the low nibble.
fn v6_tag(index: u64, tag_type: u8) -> Vec<u8> {
    v6_varuint((index << 4) | tag_type as u64)
}

fn v6_crdt(part2: u64) -> Vec<u8> {
    let mut out = vec![1u8];
    out.extend_from_slice(&v6_varuint(part2));
    out
}

/// One `SceneLineItemBlock` payload: a black ballpoint line, v2 packed points.
fn v6_line_payload(points: &[(f32, f32)]) -> Vec<u8> {
    let mut point_data = Vec::with_capacity(points.len() * 14);
    for (x, y) in points {
        point_data.extend_from_slice(&x.to_le_bytes());
        point_data.extend_from_slice(&y.to_le_bytes());
        point_data.extend_from_slice(&8i16.to_le_bytes()); // speed, quarter units
        point_data.extend_from_slice(&8i16.to_le_bytes()); // width 2.0px
        point_data.push(0); // direction
        point_data.push(255); // full pressure
    }

    let mut value = vec![ITEM_TYPE_LINE];
    value.extend_from_slice(&v6_tag(1, TAG_BYTE4));
    value.extend_from_slice(&14u32.to_le_bytes()); // ballpoint
    value.extend_from_slice(&v6_tag(2, TAG_BYTE4));
    value.extend_from_slice(&0u32.to_le_bytes()); // black
    value.extend_from_slice(&v6_tag(3, TAG_BYTE8));
    value.extend_from_slice(&2.0f64.to_le_bytes()); // thickness scale
    value.extend_from_slice(&v6_tag(4, TAG_BYTE4));
    value.extend_from_slice(&0.0f32.to_le_bytes()); // starting length
    value.extend_from_slice(&v6_tag(5, TAG_LENGTH4));
    value.extend_from_slice(&(point_data.len() as u32).to_le_bytes());
    value.extend_from_slice(&point_data);

    let mut out = Vec::new();
    for (index, node) in [(1u64, 10u64), (2, 20), (3, 0), (4, 0)] {
        out.extend_from_slice(&v6_tag(index, TAG_ID));
        out.extend_from_slice(&v6_crdt(node));
    }
    out.extend_from_slice(&v6_tag(5, TAG_BYTE4));
    out.extend_from_slice(&0u32.to_le_bytes()); // deleted length
    out.extend_from_slice(&v6_tag(6, TAG_LENGTH4));
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(&value);
    out
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
