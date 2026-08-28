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

use super::rm_parser::{
    color_from_raw, pen_from_raw, RmLayer, RmParagraph, RmPoint, RmStroke, RmText, RmTextSpan,
    TextStyle,
};

const BLOCK_HEADER_LEN: usize = 8;
const BLOCK_SCENE_LINE_ITEM: u8 = 0x05;
const BLOCK_ROOT_TEXT: u8 = 0x07;
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

/// The `(0, 0)` id doubles as the sequence end marker and as the style
/// anchor for the first paragraph of a text block.
const END_MARKER: CrdtId = (0, 0);

/// Decode the block stream that follows the 43-byte file header.
///
/// Strokes are grouped into layers by the scene-tree group they hang off,
/// preserving the order groups are first seen. Typed text lives in its own
/// root-text block; if one decodes to visible characters it is returned
/// alongside the layers.
pub fn parse_v6_blocks(body: &[u8]) -> (Vec<RmLayer>, Option<RmText>) {
    let mut groups: Vec<(CrdtId, Vec<RmStroke>)> = Vec::new();
    let mut text: Option<RmText> = None;
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
        } else if block_type == BLOCK_ROOT_TEXT {
            if let Some(parsed) = parse_root_text(&body[payload_start..payload_end]) {
                // A page normally has one root-text block; keep the latest.
                text = Some(parsed);
            }
        }

        if payload_end == offset {
            // A zero-length block with no header advance would spin forever.
            break;
        }
        offset = payload_end;
    }

    let layers = groups
        .into_iter()
        .map(|(_, strokes)| RmLayer { strokes })
        .collect();
    (layers, text.filter(|t| !t.is_empty()))
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

/// One text item as stored: a CRDT sequence entry whose value is a string
/// (of one or more characters), a deletion placeholder, or an inline
/// formatting code.
struct TextItem {
    item_id: CrdtId,
    left_id: CrdtId,
    right_id: CrdtId,
    deleted_length: u32,
    value: TextItemValue,
}

enum TextItemValue {
    Text(String),
    Format(u32),
}

/// A text item expanded to a single character, so every character has an
/// explicit id for CRDT ordering (a stored string's characters implicitly
/// take sequential ids starting at the item's own id).
struct ExpandedChar {
    id: CrdtId,
    left: CrdtId,
    right: CrdtId,
    value: CharValue,
}

#[derive(Clone, PartialEq)]
enum CharValue {
    Char(char),
    /// A deleted character: occupies its id in the sequence (other items
    /// may anchor on it) but contributes no visible text.
    Deleted,
    /// Inline format toggle: 1/2 bold on/off, 3/4 italic on/off.
    Format(u32),
}

/// Decode one `RootTextBlock` (type 0x07). Layout, per the rmscene
/// reference implementation:
///
/// ```text
/// id(1)=block id, subblock(2){
///   subblock(1){ subblock(1){ varuint count, count × text item } }
///   subblock(2){ subblock(1){ varuint count, count × paragraph style } }
/// }
/// subblock(3){ f64 pos_x, f64 pos_y }, tag(4)=f32 width
/// ```
///
/// Any structural surprise aborts the whole block (`None`) — unlike a bad
/// stroke, a half-read text stream would order characters wrongly.
fn parse_root_text(payload: &[u8]) -> Option<RmText> {
    let mut r = Reader::new(payload);

    r.expect_tag(1, TAG_ID)?;
    r.crdt_id()?; // block id, always (0, 0)

    let outer_end = r.subblock(2)?;

    let items_outer_end = r.subblock(1)?;
    let items_end = r.subblock(1)?;
    let item_count = r.varuint()? as usize;
    let mut items = Vec::new();
    for _ in 0..item_count {
        items.push(parse_text_item(&mut r)?);
    }
    r.seek(items_end)?;
    r.seek(items_outer_end)?;

    let styles_outer_end = r.subblock(2)?;
    let styles_end = r.subblock(1)?;
    let style_count = r.varuint()? as usize;
    let mut styles: Vec<(CrdtId, u8)> = Vec::new();
    for _ in 0..style_count {
        let entry = parse_text_format(&mut r)?;
        // Duplicate anchors keep the last entry, like the reference.
        styles.retain(|(id, _)| *id != entry.0);
        styles.push(entry);
    }
    r.seek(styles_end)?;
    r.seek(styles_outer_end)?;
    r.seek(outer_end)?;

    let pos_end = r.subblock(3)?;
    let pos_x = r.f64()?;
    let pos_y = r.f64()?;
    r.seek(pos_end)?;

    r.expect_tag(4, TAG_BYTE4)?;
    let width = r.f32()?;

    // A deletion cannot describe more characters than the block has bytes
    // to have stored them in; see expand_item.
    Some(assemble_text(
        items,
        &styles,
        payload.len(),
        pos_x,
        pos_y,
        width,
    ))
}

/// Decode one text item subblock. Items carrying a format code store it in
/// place of the string; deleted items store neither.
fn parse_text_item(r: &mut Reader) -> Option<TextItem> {
    let end = r.subblock(0)?;

    r.expect_tag(2, TAG_ID)?;
    let item_id = r.crdt_id()?;
    r.expect_tag(3, TAG_ID)?;
    let left_id = r.crdt_id()?;
    r.expect_tag(4, TAG_ID)?;
    let right_id = r.crdt_id()?;
    r.expect_tag(5, TAG_BYTE4)?;
    let deleted_length = r.u32()?;

    let value = if r.pos() < end && r.peek_tag() == Some((6, TAG_LENGTH4)) {
        let str_end = r.subblock(6)?;
        let len = r.varuint()? as usize;
        r.u8()?; // "is ascii" flag; the bytes are UTF-8 either way
        let text = String::from_utf8(r.take(len)?.to_vec()).ok()?;
        // An inline-format item stores its code after an empty string.
        let value = if r.pos() < str_end && r.peek_tag() == Some((2, TAG_BYTE4)) {
            r.tag()?;
            TextItemValue::Format(r.u32()?)
        } else {
            TextItemValue::Text(text)
        };
        r.seek(str_end)?;
        value
    } else {
        TextItemValue::Text(String::new())
    };

    r.seek(end)?;
    Some(TextItem {
        item_id,
        left_id,
        right_id,
        deleted_length,
        value,
    })
}

/// Decode one paragraph-style entry: which style the paragraph anchored at
/// `char_id` uses. The anchor id is stored raw, without a tag.
fn parse_text_format(r: &mut Reader) -> Option<(CrdtId, u8)> {
    let char_id = r.crdt_id()?;
    r.expect_tag(1, TAG_ID)?;
    r.crdt_id()?; // timestamp
    let end = r.subblock(2)?;
    r.u8()?; // constant 17
    let style_code = r.u8()?;
    r.seek(end)?;
    Some((char_id, style_code))
}

/// Expand items to single characters, order them by their CRDT links, and
/// split the result into styled paragraphs at newlines.
fn assemble_text(
    items: Vec<TextItem>,
    styles: &[(CrdtId, u8)],
    deleted_cap: usize,
    pos_x: f64,
    pos_y: f64,
    width: f32,
) -> RmText {
    let mut chars: Vec<ExpandedChar> = Vec::new();
    for item in items {
        expand_item(item, deleted_cap, &mut chars);
    }
    let ordered = toposort_chars(&chars);

    let style_for = |anchor: CrdtId| -> TextStyle {
        styles
            .iter()
            .find(|(id, _)| *id == anchor)
            .map(|(_, code)| style_from_code(*code))
            .unwrap_or_default()
    };

    let mut paragraphs = Vec::new();
    let mut queue: std::collections::VecDeque<&ExpandedChar> =
        ordered.iter().map(|&i| &chars[i]).collect();
    let mut bold = false;
    let mut italic = false;
    while !queue.is_empty() {
        // A paragraph is anchored by the newline that starts it; the very
        // first paragraph has no newline and anchors on the end marker.
        let anchor = match queue.front() {
            Some(c) if c.value == CharValue::Char('\n') => queue.pop_front().unwrap().id,
            _ => END_MARKER,
        };
        let mut spans: Vec<RmTextSpan> = Vec::new();
        while let Some(c) = queue.front() {
            match &c.value {
                CharValue::Char('\n') => break,
                CharValue::Char(ch) => {
                    match spans.last_mut() {
                        Some(s) if s.bold == bold && s.italic == italic => s.text.push(*ch),
                        _ => spans.push(RmTextSpan {
                            text: ch.to_string(),
                            bold,
                            italic,
                        }),
                    }
                    queue.pop_front();
                }
                CharValue::Deleted => {
                    queue.pop_front();
                }
                CharValue::Format(code) => {
                    match code {
                        1 => bold = true,
                        2 => bold = false,
                        3 => italic = true,
                        4 => italic = false,
                        _ => {}
                    }
                    queue.pop_front();
                }
            }
        }
        paragraphs.push(RmParagraph {
            style: style_for(anchor),
            spans,
        });
    }

    RmText {
        paragraphs,
        pos_x,
        pos_y,
        width,
    }
}

fn style_from_code(code: u8) -> TextStyle {
    match code {
        0 | 1 => TextStyle::Plain,
        2 => TextStyle::Heading,
        3 => TextStyle::Bold,
        4 => TextStyle::Bullet,
        5 => TextStyle::Bullet2,
        6 => TextStyle::Checkbox,
        7 => TextStyle::CheckboxChecked,
        _ => TextStyle::Plain,
    }
}

/// Expand one stored item into per-character entries. Characters within a
/// stored string take sequential ids from the item's own id; a deletion of
/// length n does the same for n placeholder entries.
///
/// `deleted_cap` bounds how many placeholders a single deletion may claim.
/// The count is a raw u32 off the wire and nothing in the file has to back
/// it, so an unbounded expansion would let a hostile page ask for a 34 GB
/// allocation — and an allocation failure aborts the process rather than
/// unwinding to the "skip malformed input" path this module relies on.
fn expand_item(item: TextItem, deleted_cap: usize, out: &mut Vec<ExpandedChar>) {
    let values: Vec<CharValue> = match item.value {
        TextItemValue::Format(code) => {
            out.push(ExpandedChar {
                id: item.item_id,
                left: item.left_id,
                right: item.right_id,
                value: CharValue::Format(code),
            });
            return;
        }
        TextItemValue::Text(s) if item.deleted_length > 0 => {
            debug_assert!(s.is_empty());
            let n = (item.deleted_length as usize).min(deleted_cap);
            vec![CharValue::Deleted; n]
        }
        TextItemValue::Text(s) => s.chars().map(CharValue::Char).collect(),
    };
    if values.is_empty() {
        return;
    }

    let last = values.len() - 1;
    let mut left = item.left_id;
    for (k, value) in values.into_iter().enumerate() {
        // Ids are wire-supplied and may sit near u64::MAX; saturating
        // keeps a crafted counter from wrapping onto another item's id
        // (or onto END_MARKER) and scrambling the ordering.
        let (author, base) = item.item_id;
        let id = (author, base.saturating_add(k as u64));
        let right = if k == last {
            item.right_id
        } else {
            (author, base.saturating_add(k as u64 + 1))
        };
        out.push(ExpandedChar {
            id,
            left,
            right,
            value,
        });
        left = id;
    }
}

/// Order expanded characters by their left/right links — Kahn's algorithm
/// with a deterministic tie-break (higher author id first, then counter),
/// mirroring the rmscene reference. Returns indices into `chars`. If the
/// links are cyclic or disconnected the leftovers are appended in file
/// order rather than dropped.
fn toposort_chars(chars: &[ExpandedChar]) -> Vec<usize> {
    use std::collections::{BinaryHeap, HashMap};

    // Node encoding: 0 = start sentinel, 1 = end sentinel, i + 2 = chars[i].
    const START: usize = 0;
    const END: usize = 1;

    let index_of: HashMap<CrdtId, usize> = chars
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i + 2))
        .collect();
    let resolve = |id: CrdtId, fallback: usize| -> usize {
        if id == END_MARKER {
            fallback
        } else {
            *index_of.get(&id).unwrap_or(&fallback)
        }
    };

    let mut in_degree = vec![0usize; chars.len() + 2];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); chars.len() + 2];
    for (i, c) in chars.iter().enumerate() {
        let node = i + 2;
        let left = resolve(c.left, START);
        let right = resolve(c.right, END);
        in_degree[node] += 1;
        dependents[left].push(node);
        in_degree[right] += 1;
        dependents[node].push(right);
    }

    // Min-heap on (rank, 255 - author, counter): the start sentinel first,
    // real characters next (higher author id winning ties), end last.
    let key = |node: usize| -> (u8, u8, u64) {
        match node {
            START => (0, 0, 0),
            END => (2, 0, 0),
            _ => {
                let (author, counter) = chars[node - 2].id;
                (1, u8::MAX - author, counter)
            }
        }
    };
    let mut ready = BinaryHeap::new();
    for (node, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            ready.push(std::cmp::Reverse((key(node), node)));
        }
    }

    let mut order = Vec::with_capacity(chars.len());
    while let Some(std::cmp::Reverse((_, node))) = ready.pop() {
        if node >= 2 {
            order.push(node - 2);
        }
        if node == END {
            break;
        }
        for &dep in &dependents[node] {
            in_degree[dep] -= 1;
            if in_degree[dep] == 0 {
                ready.push(std::cmp::Reverse((key(dep), dep)));
            }
        }
    }

    if order.len() < chars.len() {
        // Broken links (cycle or dangling reference): keep the text rather
        // than the ordering guarantee.
        let placed: std::collections::HashSet<usize> = order.iter().copied().collect();
        order.extend((0..chars.len()).filter(|i| !placed.contains(i)));
    }
    order
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

    /// Read the next tag without consuming it.
    fn peek_tag(&mut self) -> Option<(u64, u8)> {
        let saved = self.pos;
        let tag = self.tag();
        self.pos = saved;
        tag
    }

    fn pos(&self) -> usize {
        self.pos
    }

    /// Jump to an absolute offset; refuses to move backwards or past the
    /// end, either of which would mean a subblock length lied.
    fn seek(&mut self, to: usize) -> Option<()> {
        if to < self.pos || to > self.data.len() {
            return None;
        }
        self.pos = to;
        Some(())
    }

    /// Expect a length-prefixed subblock with the given field index and
    /// return the absolute offset where it ends. Callers `seek` to that
    /// offset when done, so unknown trailing fields inside the subblock
    /// are skipped rather than corrupting subsequent reads.
    fn subblock(&mut self, index: u64) -> Option<usize> {
        self.expect_tag(index, TAG_LENGTH4)?;
        let len = self.u32()? as usize;
        let end = self.pos.checked_add(len)?;
        (end <= self.data.len()).then_some(end)
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
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

        let (layers, _) = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        let s = &layers[0].strokes[0];
        assert_eq!(s.points.len(), 1, "24-byte points, not 14");
        assert!((s.points[0].x - 5.0).abs() < 0.01);
        assert!((s.points[0].width - 3.0).abs() < 0.01);
        assert!((s.points[0].pressure - 0.8).abs() < 0.01);
    }

    #[test]
    fn empty_body_yields_no_layers() {
        assert!(parse_v6_blocks(&[]).0.is_empty());
        assert!(parse_v6_blocks(&[0u8; 4]).0.is_empty());
    }

    // ---- Root text (typed text) ----

    fn crdt_full(part1: u8, part2: u64) -> Vec<u8> {
        let mut out = vec![part1];
        out.extend_from_slice(&varuint(part2));
        out
    }

    fn subblock(index: u64, inner: &[u8]) -> Vec<u8> {
        let mut out = tag(index, TAG_LENGTH4);
        out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        out.extend_from_slice(inner);
        out
    }

    /// Encode one stored text item. `text` of `Err(fmt)` writes an inline
    /// format code instead of a string.
    fn text_item(
        item_id: CrdtId,
        left: CrdtId,
        right: CrdtId,
        deleted: u32,
        value: Option<Result<&str, u32>>,
    ) -> Vec<u8> {
        let mut inner = Vec::new();
        inner.extend_from_slice(&tag(2, TAG_ID));
        inner.extend_from_slice(&crdt_full(item_id.0, item_id.1));
        inner.extend_from_slice(&tag(3, TAG_ID));
        inner.extend_from_slice(&crdt_full(left.0, left.1));
        inner.extend_from_slice(&tag(4, TAG_ID));
        inner.extend_from_slice(&crdt_full(right.0, right.1));
        inner.extend_from_slice(&tag(5, TAG_BYTE4));
        inner.extend_from_slice(&deleted.to_le_bytes());
        if let Some(value) = value {
            let mut s = Vec::new();
            let text = value.unwrap_or_default();
            s.extend_from_slice(&varuint(text.len() as u64));
            s.push(1); // "is ascii" flag
            s.extend_from_slice(text.as_bytes());
            if let Err(fmt) = value {
                s.extend_from_slice(&tag(2, TAG_BYTE4));
                s.extend_from_slice(&fmt.to_le_bytes());
            }
            inner.extend_from_slice(&subblock(6, &s));
        }
        subblock(0, &inner)
    }

    fn style_entry(anchor: CrdtId, code: u8) -> Vec<u8> {
        let mut out = crdt_full(anchor.0, anchor.1);
        out.extend_from_slice(&tag(1, TAG_ID));
        out.extend_from_slice(&crdt_full(0, 1));
        out.extend_from_slice(&subblock(2, &[17, code]));
        out
    }

    fn root_text_payload(items: &[Vec<u8>], styles: &[Vec<u8>]) -> Vec<u8> {
        let mut item_seq = varuint(items.len() as u64);
        for item in items {
            item_seq.extend_from_slice(item);
        }
        let mut style_seq = varuint(styles.len() as u64);
        for style in styles {
            style_seq.extend_from_slice(style);
        }

        let mut outer = subblock(1, &subblock(1, &item_seq));
        outer.extend_from_slice(&subblock(2, &subblock(1, &style_seq)));

        let mut payload = Vec::new();
        payload.extend_from_slice(&tag(1, TAG_ID));
        payload.extend_from_slice(&crdt_full(0, 0));
        payload.extend_from_slice(&subblock(2, &outer));
        let mut pos = Vec::new();
        pos.extend_from_slice(&(-468.0f64).to_le_bytes());
        pos.extend_from_slice(&234.0f64.to_le_bytes());
        payload.extend_from_slice(&subblock(3, &pos));
        payload.extend_from_slice(&tag(4, TAG_BYTE4));
        payload.extend_from_slice(&936.0f32.to_le_bytes());
        payload
    }

    fn text_of(text: &RmText) -> String {
        text.paragraphs
            .iter()
            .map(|p| {
                p.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parses_root_text_into_paragraphs_with_styles() {
        let payload = root_text_payload(
            &[text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("Title\nbody text")))],
            // Style anchored on END_MARKER applies to the first paragraph.
            &[style_entry(END_MARKER, 2)],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (layers, text) = parse_v6_blocks(&blob);
        assert!(layers.is_empty());
        let text = text.expect("root text should be recovered");
        assert_eq!(text_of(&text), "Title\nbody text");
        assert_eq!(text.paragraphs[0].style, TextStyle::Heading);
        assert_eq!(text.paragraphs[1].style, TextStyle::Plain);
        assert_eq!(text.pos_x, -468.0);
        assert_eq!(text.pos_y, 234.0);
        assert_eq!(text.width, 936.0);
    }

    #[test]
    fn orders_inserted_text_by_crdt_links() {
        // "AC" stored first; "B" inserted later anchors between the two
        // characters of the first item (left = id of 'A' = (1,10),
        // right = id of 'C' = (1,11)).
        let payload = root_text_payload(
            &[
                text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("AC"))),
                text_item((1, 20), (1, 10), (1, 11), 0, Some(Ok("B"))),
            ],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        assert_eq!(text_of(&text.unwrap()), "ABC");
    }

    #[test]
    fn deleted_text_is_skipped_but_still_anchors_ordering() {
        // "X" (deleted, one char at id (1,10)) then "Z" anchored after it.
        // The deleted char contributes nothing but its id must still
        // order "Z" after the start.
        let payload = root_text_payload(
            &[
                text_item((1, 10), END_MARKER, END_MARKER, 1, Some(Ok(""))),
                text_item((1, 20), (1, 10), END_MARKER, 0, Some(Ok("Z"))),
                text_item((1, 30), END_MARKER, (1, 10), 0, Some(Ok("A"))),
            ],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        assert_eq!(text_of(&text.unwrap()), "AZ");
    }

    #[test]
    fn inline_format_codes_toggle_bold_and_italic_spans() {
        let payload = root_text_payload(
            &[
                text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("a"))),
                text_item((1, 20), (1, 10), END_MARKER, 0, Some(Err(1))), // bold on
                text_item((1, 30), (1, 20), END_MARKER, 0, Some(Ok("b"))),
                text_item((1, 40), (1, 30), END_MARKER, 0, Some(Err(2))), // bold off
                text_item((1, 50), (1, 40), END_MARKER, 0, Some(Ok("c"))),
            ],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        let text = text.unwrap();
        let spans = &text.paragraphs[0].spans;
        assert_eq!(spans.len(), 3);
        assert_eq!((spans[0].text.as_str(), spans[0].bold), ("a", false));
        assert_eq!((spans[1].text.as_str(), spans[1].bold), ("b", true));
        assert_eq!((spans[2].text.as_str(), spans[2].bold), ("c", false));
    }

    #[test]
    fn non_ascii_text_survives() {
        let payload = root_text_payload(
            &[text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("héllo → ✓")))],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        assert_eq!(text_of(&text.unwrap()), "héllo → ✓");
    }

    #[test]
    fn fully_deleted_text_yields_none() {
        let payload = root_text_payload(
            &[text_item((1, 10), END_MARKER, END_MARKER, 5, Some(Ok("")))],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        assert!(text.is_none(), "deleted-only text is not renderable text");
    }

    #[test]
    fn huge_deleted_length_does_not_allocate_unboundedly() {
        // Security regression: deleted_length is a raw u32 off the wire.
        // Expanding it verbatim let a ~120-byte crafted page request a
        // 34 GB allocation, and an allocation failure aborts the process
        // instead of unwinding to the skip-malformed-input path.
        let payload = root_text_payload(
            &[
                text_item((1, 10), END_MARKER, END_MARKER, u32::MAX, Some(Ok(""))),
                text_item((1, 20), (1, 10), END_MARKER, 0, Some(Ok("ok"))),
            ],
            &[],
        );
        assert!(payload.len() < 200, "the crafted block stays tiny");

        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        // Still parses, still recovers the live text, without honouring
        // the absurd deletion count.
        assert_eq!(text_of(&text.unwrap()), "ok");
    }

    #[test]
    fn character_ids_near_u64_max_do_not_wrap() {
        // A wrapped counter could collide with END_MARKER (0, 0) and
        // scramble the ordering of unrelated characters.
        let payload = root_text_payload(
            &[text_item((0, u64::MAX - 1), END_MARKER, END_MARKER, 0, Some(Ok("abc")))],
            &[],
        );
        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (_, text) = parse_v6_blocks(&blob);
        let text = text.expect("text should still be recovered");
        assert_eq!(text.char_count(), 3);
    }

    #[test]
    fn corrupt_root_text_block_keeps_strokes() {
        let mut corrupt = root_text_payload(
            &[text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("hello")))],
            &[],
        );
        corrupt.truncate(corrupt.len() / 3);

        let blob = BlockBuilder::new()
            .raw_block(BLOCK_ROOT_TEXT, 1, &corrupt)
            .line(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]])
            .done();

        let (layers, text) = parse_v6_blocks(&blob);
        assert!(text.is_none(), "malformed text must not be half-decoded");
        assert_eq!(layers.len(), 1, "strokes still parse");
    }

    #[test]
    fn text_and_strokes_coexist() {
        let payload = root_text_payload(
            &[text_item((1, 10), END_MARKER, END_MARKER, 0, Some(Ok("note")))],
            &[],
        );
        let blob = BlockBuilder::new()
            .line(10, 14, 0, &[[1.0, 2.0, 1.0, 2.0, 0.0, 128.0]])
            .raw_block(BLOCK_ROOT_TEXT, 1, &payload)
            .done();

        let (layers, text) = parse_v6_blocks(&blob);
        assert_eq!(layers.len(), 1);
        assert_eq!(text_of(&text.unwrap()), "note");
    }
}
