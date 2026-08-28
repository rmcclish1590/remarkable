//! Parser for the reMarkable `.rm` binary stroke formats.
//!
//! Every version opens with the same 43-byte ASCII header
//! (`"reMarkable .lines file, version=N"` padded with spaces), but the
//! body differs completely between generations, so the version selects
//! the decoder:
//!
//! - **v6** (firmware 3.x and later) is a CRDT scene tree written as a
//!   sequence of length-prefixed blocks. Decoded by [`crate::remarkable::rm_v6`].
//! - **v3/v5** use the older flat layout handled here:
//!   - `i32` layer count, then for each layer:
//!     - `i32` stroke count, then for each stroke:
//!       - `i32` pen, `i32` color, `i32` (skip), `f32` base width,
//!         `i32` (skip), `i32` point count, then for each point:
//!         - 6 × `f32`: x, y, speed, direction, width, pressure.
//!
//! All formats are little-endian and uncompressed.

use super::rm_v6::parse_v6_blocks;
use nom::multi::count;
use nom::number::complete::{le_f32, le_i32};
use nom::IResult;
use std::fs;
use std::path::Path;
use thiserror::Error;

const HEADER_LEN: usize = 43;
const HEADER_PREFIX: &str = "reMarkable .lines file, version=";
const POST_HEADER_PAD: usize = 10;
const V6_VERSION: u32 = 6;
/// Versions handled by the flat layout below.
const FLAT_VERSIONS: [u32; 2] = [3, 5];

#[derive(Debug, Clone)]
pub struct RmPage {
    pub version: u32,
    pub layers: Vec<RmLayer>,
}

#[derive(Debug, Clone)]
pub struct RmLayer {
    pub strokes: Vec<RmStroke>,
}

#[derive(Debug, Clone)]
pub struct RmStroke {
    pub pen: PenType,
    pub color: PenColor,
    pub base_width: f32,
    pub points: Vec<RmPoint>,
}

#[derive(Debug, Clone)]
pub struct RmPoint {
    pub x: f32,
    pub y: f32,
    pub speed: f32,
    pub direction: f32,
    pub width: f32,
    pub pressure: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenType {
    BallPoint,
    Marker,
    Fineliner,
    SharpPencil,
    TiltPencil,
    Brush,
    Highlighter,
    Eraser,
    EraseArea,
    CalligraphyPen,
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenColor {
    Black,
    Grey,
    White,
    Yellow,
    Green,
    Pink,
    Blue,
    Red,
    GrayOverlap,
    Unknown(u32),
}

#[derive(Debug, Error)]
pub enum RmParseError {
    #[error("Invalid header: expected reMarkable .lines file")]
    InvalidHeader,
    #[error("Unsupported version: {0} (expected 3, 5 or 6)")]
    UnsupportedVersion(u32),
    #[error("Unexpected end of file at offset {0}")]
    UnexpectedEof(usize),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn parse_rm_file(input: &[u8]) -> Result<RmPage, RmParseError> {
    let version = parse_header(input)?;

    if version == V6_VERSION {
        // v6 blocks begin immediately after the header, with no padding.
        let layers = parse_v6_blocks(&input[HEADER_LEN..]);
        return Ok(RmPage { version, layers });
    }

    if !FLAT_VERSIONS.contains(&version) {
        return Err(RmParseError::UnsupportedVersion(version));
    }

    // The layer count normally follows the header directly, but some
    // writers insert padding first. Reading at the wrong offset usually
    // still "succeeds" with a nonsense layer count rather than erroring,
    // so decode both ways and keep whichever actually recovered strokes.
    let unpadded = parse_flat_body(input, version, HEADER_LEN);
    let padded = parse_flat_body(input, version, HEADER_LEN + POST_HEADER_PAD);
    match (unpadded, padded) {
        (Ok(a), Ok(b)) => Ok(if b.total_strokes() > a.total_strokes() {
            b
        } else {
            a
        }),
        (Ok(page), Err(_)) | (Err(_), Ok(page)) => Ok(page),
        (Err(e), Err(_)) => Err(e),
    }
}

fn parse_flat_body(
    input: &[u8],
    version: u32,
    body_start: usize,
) -> Result<RmPage, RmParseError> {
    let rest = input
        .get(body_start..)
        .ok_or(RmParseError::UnexpectedEof(body_start))?;

    let (rest, num_layers) = le_i32::<_, nom::error::Error<&[u8]>>(rest)
        .map_err(|_| RmParseError::UnexpectedEof(body_start))?;
    let num_layers = checked_count(num_layers, "layer count", body_start)?;

    let offset_at_layers = input.len() - rest.len();
    let (_, layers) = count(parse_layer, num_layers)(rest)
        .map_err(|_| RmParseError::UnexpectedEof(offset_at_layers))?;

    Ok(RmPage { version, layers })
}

pub fn parse_rm_from_path(path: &Path) -> Result<RmPage, RmParseError> {
    let bytes = fs::read(path)?;
    parse_rm_file(&bytes)
}

impl RmPage {
    pub fn bounding_box(&self) -> (f32, f32, f32, f32) {
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut seen = false;
        for layer in &self.layers {
            for stroke in &layer.strokes {
                for p in &stroke.points {
                    seen = true;
                    if p.x < min_x {
                        min_x = p.x;
                    }
                    if p.y < min_y {
                        min_y = p.y;
                    }
                    if p.x > max_x {
                        max_x = p.x;
                    }
                    if p.y > max_y {
                        max_y = p.y;
                    }
                }
            }
        }
        if seen {
            (min_x, min_y, max_x, max_y)
        } else {
            (0.0, 0.0, 0.0, 0.0)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total_strokes() == 0
    }

    pub fn total_strokes(&self) -> usize {
        self.layers.iter().map(|l| l.strokes.len()).sum()
    }

    pub fn total_points(&self) -> usize {
        self.layers
            .iter()
            .flat_map(|l| l.strokes.iter())
            .map(|s| s.points.len())
            .sum()
    }
}

fn parse_header(input: &[u8]) -> Result<u32, RmParseError> {
    if input.len() < HEADER_LEN {
        return Err(RmParseError::UnexpectedEof(0));
    }
    let header_bytes = &input[..HEADER_LEN];
    let header_str =
        std::str::from_utf8(header_bytes).map_err(|_| RmParseError::InvalidHeader)?;
    let version_field = header_str
        .strip_prefix(HEADER_PREFIX)
        .ok_or(RmParseError::InvalidHeader)?;
    let version_digits: String = version_field
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if version_digits.is_empty() {
        return Err(RmParseError::InvalidHeader);
    }
    let version: u32 = version_digits.parse().map_err(|_| RmParseError::InvalidHeader)?;
    Ok(version)
}

fn parse_layer(input: &[u8]) -> IResult<&[u8], RmLayer> {
    let (i, num_strokes) = le_i32(input)?;
    let n = num_strokes.max(0) as usize;
    let (i, strokes) = count(parse_stroke, n)(i)?;
    Ok((i, RmLayer { strokes }))
}

fn parse_stroke(input: &[u8]) -> IResult<&[u8], RmStroke> {
    let (i, pen_raw) = le_i32(input)?;
    let (i, color_raw) = le_i32(i)?;
    let (i, _pad_a) = le_i32(i)?;
    let (i, base_width) = le_f32(i)?;
    let (i, _pad_b) = le_i32(i)?;
    let (i, num_points) = le_i32(i)?;
    let n = num_points.max(0) as usize;
    let (i, points) = count(parse_point, n)(i)?;
    Ok((
        i,
        RmStroke {
            pen: pen_from_raw(pen_raw as u32),
            color: color_from_raw(color_raw as u32),
            base_width,
            points,
        },
    ))
}

fn parse_point(input: &[u8]) -> IResult<&[u8], RmPoint> {
    let (i, x) = le_f32(input)?;
    let (i, y) = le_f32(i)?;
    let (i, speed) = le_f32(i)?;
    let (i, direction) = le_f32(i)?;
    let (i, width) = le_f32(i)?;
    let (i, pressure) = le_f32(i)?;
    Ok((
        i,
        RmPoint {
            x,
            y,
            speed,
            direction,
            width,
            pressure,
        },
    ))
}

fn checked_count(raw: i32, label: &str, offset: usize) -> Result<usize, RmParseError> {
    if raw < 0 {
        Err(RmParseError::InvalidData(format!(
            "negative {label} {raw} at offset {offset}"
        )))
    } else {
        Ok(raw as usize)
    }
}

pub(super) fn pen_from_raw(n: u32) -> PenType {
    match n {
        0 | 1 | 12 | 14 => PenType::BallPoint,
        2 | 4 | 15 | 17 => PenType::Fineliner,
        3 | 16 => PenType::Marker,
        5 | 18 => PenType::Highlighter,
        6 => PenType::Eraser,
        7 => PenType::SharpPencil,
        8 => PenType::EraseArea,
        9 | 10 => PenType::TiltPencil,
        11 | 13 => PenType::Brush,
        21 => PenType::CalligraphyPen,
        _ => PenType::Unknown(n),
    }
}

pub(super) fn color_from_raw(n: u32) -> PenColor {
    match n {
        0 => PenColor::Black,
        1 => PenColor::Grey,
        2 => PenColor::White,
        3 => PenColor::Yellow,
        4 => PenColor::Green,
        5 => PenColor::Pink,
        6 => PenColor::Blue,
        7 => PenColor::Red,
        8 => PenColor::GrayOverlap,
        _ => PenColor::Unknown(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlobBuilder {
        buf: Vec<u8>,
    }

    impl BlobBuilder {
        fn new(version: u32) -> Self {
            let mut buf = Vec::new();
            let header = format!("{HEADER_PREFIX}{version}");
            buf.extend_from_slice(header.as_bytes());
            buf.resize(HEADER_LEN, b' ');
            buf.extend_from_slice(&[0u8; POST_HEADER_PAD]);
            Self { buf }
        }
        fn i32(mut self, v: i32) -> Self {
            self.buf.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn f32(mut self, v: f32) -> Self {
            self.buf.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn stroke(mut self, pen: i32, color: i32, width: f32, points: &[[f32; 6]]) -> Self {
            self = self.i32(pen).i32(color).i32(0).f32(width).i32(0).i32(points.len() as i32);
            for p in points {
                for v in p {
                    self = self.f32(*v);
                }
            }
            self
        }
        fn done(self) -> Vec<u8> {
            self.buf
        }
    }

    #[test]
    fn parses_minimal_valid_blob() {
        let blob = BlobBuilder::new(5)
            .i32(1) // 1 layer
            .i32(1) // 1 stroke
            .stroke(
                0, // BallPoint
                3, // Yellow
                2.5,
                &[
                    [10.0, 20.0, 1.0, 0.5, 2.0, 0.9],
                    [15.0, 25.0, 1.1, 0.4, 2.1, 0.8],
                    [20.0, 30.0, 1.2, 0.3, 2.2, 0.7],
                ],
            )
            .done();
        let page = parse_rm_file(&blob).unwrap();
        assert_eq!(page.version, 5);
        assert_eq!(page.layers.len(), 1);
        assert_eq!(page.layers[0].strokes.len(), 1);
        let s = &page.layers[0].strokes[0];
        assert_eq!(s.pen, PenType::BallPoint);
        assert_eq!(s.color, PenColor::Yellow);
        assert!((s.base_width - 2.5).abs() < f32::EPSILON);
        assert_eq!(s.points.len(), 3);
        assert!((s.points[1].x - 15.0).abs() < f32::EPSILON);
        assert!((s.points[1].pressure - 0.8).abs() < f32::EPSILON);
        assert_eq!(page.total_strokes(), 1);
        assert_eq!(page.total_points(), 3);
        assert!(!page.is_empty());
        let (min_x, min_y, max_x, max_y) = page.bounding_box();
        assert!((min_x - 10.0).abs() < f32::EPSILON);
        assert!((min_y - 20.0).abs() < f32::EPSILON);
        assert!((max_x - 20.0).abs() < f32::EPSILON);
        assert!((max_y - 30.0).abs() < f32::EPSILON);
    }

    #[test]
    fn empty_page_parses_to_zero_layers() {
        let blob = BlobBuilder::new(5).i32(0).done();
        let page = parse_rm_file(&blob).unwrap();
        assert_eq!(page.version, 5);
        assert!(page.layers.is_empty());
        assert!(page.is_empty());
        assert_eq!(page.total_strokes(), 0);
        assert_eq!(page.total_points(), 0);
        assert_eq!(page.bounding_box(), (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn multiple_layers_and_strokes() {
        let blob = BlobBuilder::new(5)
            .i32(2) // 2 layers
            // layer 0: 2 strokes
            .i32(2)
            .stroke(3, 0, 1.0, &[[0.0, 0.0, 0.0, 0.0, 1.0, 0.5]])
            .stroke(7, 1, 0.5, &[[1.0, 1.0, 0.0, 0.0, 0.5, 0.5]; 2])
            // layer 1: 1 stroke
            .i32(1)
            .stroke(11, 7, 3.0, &[[2.0, 2.0, 0.0, 0.0, 3.0, 0.9]; 4])
            .done();
        let page = parse_rm_file(&blob).unwrap();
        assert_eq!(page.layers.len(), 2);
        assert_eq!(page.layers[0].strokes.len(), 2);
        assert_eq!(page.layers[1].strokes.len(), 1);
        assert_eq!(page.layers[0].strokes[0].pen, PenType::Marker);
        assert_eq!(page.layers[0].strokes[1].pen, PenType::SharpPencil);
        assert_eq!(page.layers[1].strokes[0].pen, PenType::Brush);
        assert_eq!(page.layers[1].strokes[0].color, PenColor::Red);
        assert_eq!(page.total_strokes(), 3);
        assert_eq!(page.total_points(), 1 + 2 + 4);
    }

    #[test]
    fn invalid_header_errors() {
        let mut blob = vec![0u8; HEADER_LEN + POST_HEADER_PAD + 4];
        blob[..10].copy_from_slice(b"not-remark");
        let err = parse_rm_file(&blob).unwrap_err();
        assert!(matches!(err, RmParseError::InvalidHeader));
    }

    #[test]
    fn wrong_version_errors() {
        let blob = BlobBuilder::new(4).i32(0).done();
        let err = parse_rm_file(&blob).unwrap_err();
        match err {
            RmParseError::UnsupportedVersion(v) => assert_eq!(v, 4),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn v3_uses_the_flat_layout() {
        let blob = BlobBuilder::new(3)
            .i32(1)
            .i32(1)
            .stroke(0, 0, 2.0, &[[1.0, 2.0, 0.0, 0.0, 2.0, 0.5]])
            .done();
        let page = parse_rm_file(&blob).unwrap();
        assert_eq!(page.version, 3);
        assert_eq!(page.total_strokes(), 1);
    }

    #[test]
    fn flat_layout_parses_without_post_header_padding() {
        // Same content as the padded builder, but with the layer count
        // sitting immediately after the 43-byte header.
        let mut buf = Vec::new();
        let header = format!("{HEADER_PREFIX}5");
        buf.extend_from_slice(header.as_bytes());
        buf.resize(HEADER_LEN, b' ');
        buf.extend_from_slice(&1i32.to_le_bytes()); // 1 layer
        buf.extend_from_slice(&1i32.to_le_bytes()); // 1 stroke
        for v in [0i32, 0, 0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.extend_from_slice(&2.0f32.to_le_bytes());
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(&1i32.to_le_bytes()); // 1 point
        for v in [7.0f32, 8.0, 0.0, 0.0, 2.0, 0.5] {
            buf.extend_from_slice(&v.to_le_bytes());
        }

        let page = parse_rm_file(&buf).unwrap();
        assert_eq!(page.total_strokes(), 1);
        assert!((page.layers[0].strokes[0].points[0].x - 7.0).abs() < f32::EPSILON);
    }

    #[test]
    fn v6_header_routes_to_the_block_parser() {
        // The flat layout must not be applied to a v6 file: these bytes are
        // a valid v6 block stream, which the flat reader would misread as a
        // huge layer count.
        let mut buf = Vec::new();
        let header = format!("{HEADER_PREFIX}6");
        buf.extend_from_slice(header.as_bytes());
        buf.resize(HEADER_LEN, b' ');
        // One migration-info block: 7 bytes of payload, type 0x00.
        buf.extend_from_slice(&7u32.to_le_bytes());
        buf.extend_from_slice(&[0, 1, 1, 0x00]);
        buf.extend_from_slice(&[0u8; 7]);

        let page = parse_rm_file(&buf).unwrap();
        assert_eq!(page.version, 6);
        assert!(page.is_empty(), "no line blocks => no strokes");
    }

    #[test]
    fn truncated_file_errors() {
        let blob = vec![0u8; 10];
        let err = parse_rm_file(&blob).unwrap_err();
        assert!(matches!(err, RmParseError::UnexpectedEof(_)));
    }

    #[test]
    fn pen_type_mapping_covers_spec() {
        for (raw, expected) in [
            (0, PenType::BallPoint),
            (12, PenType::BallPoint),
            (1, PenType::BallPoint),
            (14, PenType::BallPoint),
            (2, PenType::Fineliner),
            (15, PenType::Fineliner),
            (4, PenType::Fineliner),
            (17, PenType::Fineliner),
            (3, PenType::Marker),
            (16, PenType::Marker),
            (5, PenType::Highlighter),
            (18, PenType::Highlighter),
            (6, PenType::Eraser),
            (7, PenType::SharpPencil),
            (8, PenType::EraseArea),
            (9, PenType::TiltPencil),
            (10, PenType::TiltPencil),
            (11, PenType::Brush),
            (13, PenType::Brush),
            (21, PenType::CalligraphyPen),
        ] {
            assert_eq!(pen_from_raw(raw), expected, "raw={raw}");
        }
        assert_eq!(pen_from_raw(999), PenType::Unknown(999));
    }

    #[test]
    fn color_mapping_covers_spec() {
        assert_eq!(color_from_raw(0), PenColor::Black);
        assert_eq!(color_from_raw(1), PenColor::Grey);
        assert_eq!(color_from_raw(2), PenColor::White);
        assert_eq!(color_from_raw(8), PenColor::GrayOverlap);
        assert_eq!(color_from_raw(42), PenColor::Unknown(42));
    }
}
