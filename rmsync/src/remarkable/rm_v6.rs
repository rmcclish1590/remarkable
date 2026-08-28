//! Parser for the reMarkable v6 `.rm` scene format.
//!
//! Despite sharing the `reMarkable .lines file, version=N` header with the
//! older formats, v6 files are not a flat list of layers and strokes. They
//! are a sequence of length-prefixed blocks describing a CRDT scene tree.
//! Only what rendering needs is decoded — line (stroke) items and the group
//! each belongs to. Text, glyphs, tombstones and tree bookkeeping are
//! skipped.
//!
//! Block header (8 bytes): `u32` payload length, `u8` unknown, `u8` minimum
//! version, `u8` current version, `u8` block type. Payload fields are
//! tagged: a varuint whose upper bits are the field index and whose low
//! nibble is the type (`0xF` CRDT id, `0xC` length-prefixed subblock,
//! `0x8` eight bytes, `0x4` four bytes).
//!
//! A malformed line block is skipped rather than failing the page: a single
//! unreadable stroke should not cost the reader every other stroke on it.

use super::rm_parser::{color_from_raw, pen_from_raw, RmLayer, RmPoint, RmStroke};

const BLOCK_HEADER_LEN: usize = 8;
const BLOCK_SCENE_LINE_ITEM: u8 = 0x05;
const ITEM_TYPE_LINE: u8 = 0x03;

const TAG_ID: u8 = 0xF;
const TAG_LENGTH4: u8 = 0xC;
const TAG_BYTE8: u8 = 0x8;
const TAG_BYTE4: u8 = 0x4;

/// Point layouts: v1 stores six floats, v2 packs the trailing four fields.
const POINT_SIZE_V1: usize = 24;
const POINT_SIZE_V2: usize = 14;

/// Identifies a scene-tree node: an author id plus a per-author counter.
type CrdtId = (u8, u64);

/// Decode the block stream that follows the 43-byte file header.
///
/// Strokes are grouped into layers by the scene-tree group they hang off,
/// preserving the order groups are first seen.
pub fn parse_v6_blocks(body: &[u8]) -> Vec<RmLayer> {
    let mut groups: Vec<(CrdtId, Vec<RmStroke>)> = Vec::new();
    let mut offset = 0usize;

    while offset + BLOCK_HEADER_LEN <= body.len() {
        let header = &body[offset..offset + BLOCK_HEADER_LEN];
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let block_version = header[6];
        let block_type = header[7];

        let payload_start = offset + BLOCK_HEADER_LEN;
        let Some(payload_end) = payload_start.checked_add(payload_len) else {
            break;
        };
        if payload_end > body.len() {
            // Truncated final block — keep whatever parsed cleanly.
            break;
        }

        if block_type == BLOCK_SCENE_LINE_ITEM {
            if let Some((parent, stroke)) =
                parse_line_item(&body[payload_start..payload_end], block_version)
            {
                match groups.iter_mut().find(|(id, _)| *id == parent) {
                    Some((_, strokes)) => strokes.push(stroke),
                    None => groups.push((parent, vec![stroke])),
                }
            }
        }

        if payload_end == offset {
            // A zero-length block with no header advance would spin forever.
            break;
        }
        offset = payload_end;
    }

    groups
        .into_iter()
        .map(|(_, strokes)| RmLayer { strokes })
        .collect()
}

/// Decode one `SceneLineItemBlock`. Returns `None` for deleted items, items
/// that are not lines, and anything that fails to decode.
fn parse_line_item(payload: &[u8], block_version: u8) -> Option<(CrdtId, RmStroke)> {
    let mut r = Reader::new(payload);

    r.expect_tag(1, TAG_ID)?;
    let parent = r.crdt_id()?;
    r.expect_tag(2, TAG_ID)?;
    r.crdt_id()?; // item id
    r.expect_tag(3, TAG_ID)?;
    r.crdt_id()?; // left sibling
    r.expect_tag(4, TAG_ID)?;
    r.crdt_id()?; // right sibling
    r.expect_tag(5, TAG_BYTE4)?;
    r.u32()?; // deleted length

    // Deleted items carry no value subblock at all.
    let (index, tag_type) = r.tag()?;
    if index != 6 || tag_type != TAG_LENGTH4 {
        return None;
    }
    r.u32()?; // subblock length
    if r.u8()? != ITEM_TYPE_LINE {
        return None;
    }

    r.expect_tag(1, TAG_BYTE4)?;
    let tool = r.u32()?;
    r.expect_tag(2, TAG_BYTE4)?;
    let color = r.u32()?;
    r.expect_tag(3, TAG_BYTE8)?;
    let thickness_scale = r.f64()?;
    r.expect_tag(4, TAG_BYTE4)?;
    r.f32()?; // starting length
    r.expect_tag(5, TAG_LENGTH4)?;
    let data_len = r.u32()? as usize;

    let point_size = if block_version == 1 {
        POINT_SIZE_V1
    } else {
        POINT_SIZE_V2
    };
    if !data_len.is_multiple_of(point_size) {
        return None;
    }
    let count = data_len / point_size;
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        points.push(read_point(&mut r, block_version)?);
    }

    // The polyline renderer draws at `base_width`, so prefer the measured
    // per-point width over the thickness multiplier, which is far too thin
    // to use as a pixel width on its own.
    let base_width = mean_width(&points).unwrap_or(thickness_scale as f32);

    Some((
        parent,
        RmStroke {
            pen: pen_from_raw(tool),
            color: color_from_raw(color),
            base_width,
            points,
        },
    ))
}

fn mean_width(points: &[RmPoint]) -> Option<f32> {
    let widths: Vec<f32> = points.iter().map(|p| p.width).filter(|w| *w > 0.0).collect();
    if widths.is_empty() {
        return None;
    }
    Some(widths.iter().sum::<f32>() / widths.len() as f32)
}

/// Read one point, normalising both layouts to the same physical units the
/// renderer expects: width in screen pixels, pressure 0–1, direction in
/// radians.
fn read_point(r: &mut Reader, block_version: u8) -> Option<RmPoint> {
    let x = r.f32()?;
    let y = r.f32()?;
    if block_version == 1 {
        Some(RmPoint {
            x,
            y,
            speed: r.f32()?,
            direction: r.f32()?,
            width: r.f32()?,
            pressure: r.f32()?,
        })
    } else {
        // v2 packs speed and width as quarter units, direction as 255 steps
        // over a full turn, and pressure as 0–255.
        let speed = r.i16()? as f32 / 4.0;
        let width = r.i16()? as f32 / 4.0;
        let direction = r.u8()? as f32 * std::f32::consts::TAU / 255.0;
        let pressure = r.u8()? as f32 / 255.0;
        Some(RmPoint {
            x,
            y,
            speed,
            direction,
            width,
            pressure,
        })
    }
}

/// Little-endian cursor over a block payload. Every read is bounds-checked
/// and yields `None` past the end, so a corrupt block unwinds to a skip.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn varuint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift >= 64 {
                return None;
            }
            result |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
        }
    }

    fn tag(&mut self) -> Option<(u64, u8)> {
        let raw = self.varuint()?;
        Some((raw >> 4, (raw & 0xF) as u8))
    }

    fn expect_tag(&mut self, index: u64, tag_type: u8) -> Option<()> {
        let (i, t) = self.tag()?;
        (i == index && t == tag_type).then_some(())
    }

    fn crdt_id(&mut self) -> Option<CrdtId> {
        let part1 = self.u8()?;
        let part2 = self.varuint()?;
        Some((part1, part2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remarkable::rm_parser::{PenColor, PenType};

    /// Builds v6 block streams the way the tablet writes them.
    struct BlockBuilder {
        buf: Vec<u8>,
    }

    impl BlockBuilder {
        fn new() -> Self {
            Self { buf: Vec::new() }
        }

        fn raw_block(mut self, block_type: u8, version: u8, payload: &[u8]) -> Self {
            self.buf
                .extend_from_slice(&(payload.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(&[0, 1, version, block_type]);
            self.buf.extend_from_slice(payload);
            self
        }

        fn line(self, parent: u64, tool: u32, color: u32, points: &[[f32; 6]]) -> Self {
            self.raw_block(BLOCK_SCENE_LINE_ITEM, 2, &line_payload(parent, tool, color, points))
        }

        fn done(self) -> Vec<u8> {
            self.buf
        }
    }

    fn varuint(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut v = v;
        loop {
            let mut byte = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                return out;
            }
        }
    }

    fn tag(index: u64, tag_type: u8) -> Vec<u8> {
        varuint((index << 4) | tag_type as u64)
    }

    fn crdt(part2: u64) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend_from_slice(&varuint(part2));
        out
    }

    /// Encodes a line item; points are `[x, y, speed, width, direction, pressure]`
    /// in the v2 packed layout.
    fn line_payload(parent: u64, tool: u32, color: u32, points: &[[f32; 6]]) -> Vec<u8> {
        let mut point_data = Vec::new();
        for p in points {
            point_data.extend_from_slice(&p[0].to_le_bytes());
            point_data.extend_from_slice(&p[1].to_le_bytes());
            point_data.extend_from_slice(&((p[2] * 4.0) as i16).to_le_bytes());
            point_data.extend_from_slice(&((p[3] * 4.0) as i16).to_le_bytes());
            point_data.push(p[4] as u8);
            point_data.push(p[5] as u8);
        }

        let mut value = vec![ITEM_TYPE_LINE];
        value.extend_from_slice(&tag(1, TAG_BYTE4));
        value.extend_from_slice(&tool.to_le_bytes());
        value.extend_from_slice(&tag(2, TAG_BYTE4));
        value.extend_from_slice(&color.to_le_bytes());
        value.extend_from_slice(&tag(3, TAG_BYTE8));
        value.extend_from_slice(&2.0f64.to_le_bytes());
        value.extend_from_slice(&tag(4, TAG_BYTE4));
        value.extend_from_slice(&0.0f32.to_le_bytes());
        value.extend_from_slice(&tag(5, TAG_LENGTH4));
        value.extend_from_slice(&(point_data.len() as u32).to_le_bytes());
        value.extend_from_slice(&point_data);

        let mut out = Vec::new();
        out.extend_from_slice(&tag(1, TAG_ID));
        out.extend_from_slice(&crdt(parent));
        out.extend_from_slice(&tag(2, TAG_ID));
        out.extend_from_slice(&crdt(20));
        out.extend_from_slice(&tag(3, TAG_ID));
        out.extend_from_slice(&crdt(0));
        out.extend_from_slice(&tag(4, TAG_ID));
        out.extend_from_slice(&crdt(0));
        out.extend_from_slice(&tag(5, TAG_BYTE4));
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&tag(6, TAG_LENGTH4));
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&value);
        out
    }

    /// A deleted line item: everything up to the value subblock, then nothing.
    fn deleted_line_payload(parent: u64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&tag(1, TAG_ID));
        out.extend_from_slice(&crdt(parent));
        out.extend_from_slice(&tag(2, TAG_ID));
        out.extend_from_slice(&crdt(21));
        out.extend_from_slice(&tag(3, TAG_ID));
        out.extend_from_slice(&crdt(0));
        out.extend_from_slice(&tag(4, TAG_ID));
        out.extend_from_slice(&crdt(0));
        out.extend_from_slice(&tag(5, TAG_BYTE4));
        out.extend_from_slice(&3u32.to_le_bytes());
        out
    }

    #[test]
    fn parses_single_line_block() {
        let blob = BlockBuilder::new()
            .line(
                10,
                14, // BallPoint
                0,  // Black
                &[
                    [100.0, 200.0, 1.0, 6.0, 0.0, 255.0],
                    [110.0, 210.0, 1.0, 6.0, 0.0, 255.0],
                ],
            )
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].strokes.len(), 1);
        let s = &layers[0].strokes[0];
        assert_eq!(s.pen, PenType::BallPoint);
        assert_eq!(s.color, PenColor::Black);
        assert_eq!(s.points.len(), 2);
        assert!((s.points[0].x - 100.0).abs() < 0.01);
        assert!((s.points[1].y - 210.0).abs() < 0.01);
        // Packed width round-trips through quarter units.
        assert!((s.points[0].width - 6.0).abs() < 0.01);
        assert!((s.points[0].pressure - 1.0).abs() < 0.01);
        // base_width is taken from the measured widths, not thickness_scale.
        assert!((s.base_width - 6.0).abs() < 0.01);
    }

    #[test]
    fn groups_strokes_by_parent_into_layers() {
        let pts = [[0.0, 0.0, 1.0, 2.0, 0.0, 128.0]];
        let blob = BlockBuilder::new()
            .line(10, 14, 0, &pts)
            .line(11, 14, 0, &pts)
            .line(10, 14, 0, &pts)
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 2, "two distinct parents => two layers");
        assert_eq!(layers[0].strokes.len(), 2, "parent 10 seen twice");
        assert_eq!(layers[1].strokes.len(), 1);
    }

    #[test]
    fn skips_deleted_line_items() {
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_SCENE_LINE_ITEM, 2, &deleted_line_payload(10))
            .line(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]])
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].strokes.len(), 1, "only the live stroke survives");
    }

    #[test]
    fn ignores_non_line_block_types() {
        let blob = BlockBuilder::new()
            .raw_block(0x00, 1, &[0u8; 7]) // migration info
            .raw_block(0x0A, 1, &[0u8; 16]) // page info
            .raw_block(0x0D, 1, &[0u8; 44]) // newer scene info
            .line(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]])
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].strokes.len(), 1);
    }

    #[test]
    fn corrupt_line_block_is_skipped_not_fatal() {
        let mut corrupt = line_payload(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]]);
        corrupt.truncate(corrupt.len() / 2);

        let blob = BlockBuilder::new()
            .raw_block(BLOCK_SCENE_LINE_ITEM, 2, &corrupt)
            .line(11, 14, 0, &[[3.0, 4.0, 1.0, 2.0, 0.0, 128.0]])
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1, "the good stroke still renders");
        assert_eq!(layers[0].strokes.len(), 1);
        assert!((layers[0].strokes[0].points[0].x - 3.0).abs() < 0.01);
    }

    #[test]
    fn truncated_trailing_block_keeps_earlier_strokes() {
        let mut blob = BlockBuilder::new()
            .line(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]])
            .done();
        // A header promising far more payload than remains.
        blob.extend_from_slice(&9999u32.to_le_bytes());
        blob.extend_from_slice(&[0, 1, 2, BLOCK_SCENE_LINE_ITEM]);
        blob.extend_from_slice(&[0u8; 4]);

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].strokes.len(), 1);
    }

    #[test]
    fn parses_v1_point_layout() {
        let mut point_data = Vec::new();
        for v in [5.0f32, 6.0, 1.5, 0.25, 3.0, 0.8] {
            point_data.extend_from_slice(&v.to_le_bytes());
        }
        let mut value = vec![ITEM_TYPE_LINE];
        value.extend_from_slice(&tag(1, TAG_BYTE4));
        value.extend_from_slice(&14u32.to_le_bytes());
        value.extend_from_slice(&tag(2, TAG_BYTE4));
        value.extend_from_slice(&0u32.to_le_bytes());
        value.extend_from_slice(&tag(3, TAG_BYTE8));
        value.extend_from_slice(&2.0f64.to_le_bytes());
        value.extend_from_slice(&tag(4, TAG_BYTE4));
        value.extend_from_slice(&0.0f32.to_le_bytes());
        value.extend_from_slice(&tag(5, TAG_LENGTH4));
        value.extend_from_slice(&(point_data.len() as u32).to_le_bytes());
        value.extend_from_slice(&point_data);

        let mut payload = Vec::new();
        payload.extend_from_slice(&tag(1, TAG_ID));
        payload.extend_from_slice(&crdt(10));
        payload.extend_from_slice(&tag(2, TAG_ID));
        payload.extend_from_slice(&crdt(20));
        payload.extend_from_slice(&tag(3, TAG_ID));
        payload.extend_from_slice(&crdt(0));
        payload.extend_from_slice(&tag(4, TAG_ID));
        payload.extend_from_slice(&crdt(0));
        payload.extend_from_slice(&tag(5, TAG_BYTE4));
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&tag(6, TAG_LENGTH4));
        payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
        payload.extend_from_slice(&value);

        let blob = BlockBuilder::new()
            .raw_block(BLOCK_SCENE_LINE_ITEM, 1, &payload)
            .done();

        let layers = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        let s = &layers[0].strokes[0];
        assert_eq!(s.points.len(), 1, "24-byte points, not 14");
        assert!((s.points[0].x - 5.0).abs() < 0.01);
        assert!((s.points[0].width - 3.0).abs() < 0.01);
        assert!((s.points[0].pressure - 0.8).abs() < 0.01);
    }

    #[test]
    fn empty_body_yields_no_layers() {
        assert!(parse_v6_blocks(&[]).is_empty());
        assert!(parse_v6_blocks(&[0u8; 4]).is_empty());
    }
}
