# Spec 03 — reMarkable .rm v6 Binary Parser

**Layer:** 0 — Foundation  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 2–3 hours  

## Objective

Implement a parser for the reMarkable `.rm` v6 binary file format that extracts all stroke data (layers, strokes, points with pressure/tilt/speed) into Rust structs suitable for SVG rendering.

## Context

Each page of a reMarkable notebook is stored as a separate `.rm` file. The format is a binary, little-endian, uncompressed format structured like C structs. Version 6 was introduced with firmware 3.0 and includes text support. Our parser must handle v6 files. Reference implementations exist in Python (`rmscene` library at https://github.com/ricklupton/rmscene) — study that repo's parsing logic for format details.

## Technical Requirements

### 1. Data model (`src/remarkable/rm_parser.rs`)

```rust
/// The top-level parsed representation of a .rm page file.
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
    pub x: f32,          // 0.0 to 1404.0 (device width in pixels)
    pub y: f32,          // 0.0 to 1872.0 (device height in pixels)
    pub speed: f32,
    pub direction: f32,  // Tilt direction
    pub width: f32,      // Pressure-adjusted width
    pub pressure: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

#[derive(Debug, Clone, Copy, PartialEq)]
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
```

### 2. Binary format specification

The .rm v6 file is structured as follows (all little-endian):

```
[Header]
  - 43 bytes: "reMarkable .lines file, version=6          " (padded with spaces/nulls)
  - 10 bytes: additional padding (may vary — read until consistent offset)

[Page data]
  - i32: number of layers

  [For each layer]
    - i32: number of strokes

    [For each stroke]
      - i32: pen type (see enum mapping below)
      - i32: color (0=black, 1=grey, 2=white, 3+=colors)
      - i32: unknown/padding (skip 4 bytes)
      - f32: base stroke width
      - i32: unknown/padding (skip 4 bytes — sometimes stroke transform)
      - i32: number of points

      [For each point]
        - f32: x coordinate
        - f32: y coordinate
        - f32: speed
        - f32: direction (tilt)
        - f32: width (pressure-adjusted)
        - f32: pressure
```

> **Important:** The exact byte offsets may vary slightly between firmware versions. The parser should validate the header string and handle minor variations gracefully. Consult `rmscene` source code for the authoritative v6 layout.

### 3. Pen type mapping

```
0, 12  → BallPoint
1, 14  → BallPoint (v2)
2, 15  → Fineliner
3, 16  → Marker
4, 17  → Fineliner (v2 — thin)
5, 18  → Highlighter
6      → Eraser
7      → SharpPencil
8      → EraseArea
9, 10  → TiltPencil
11, 13 → Brush
21     → CalligraphyPen
```

### 4. Parser implementation

Use the `nom` crate for binary parsing:

```rust
pub fn parse_rm_file(input: &[u8]) -> Result<RmPage>
```

Implementation steps:
1. Validate header — check that the file starts with `"reMarkable .lines file, version="`. Extract version number.
2. If version != 6, return an error with the detected version number.
3. Skip header padding to reach page data offset.
4. Parse layer count (i32).
5. For each layer, parse stroke count, then each stroke's metadata and point array.
6. Map pen type integers to the `PenType` enum. Unknown values → `PenType::Unknown(n)`.
7. Map color integers to `PenColor` enum. Unknown values → `PenColor::Unknown(n)`.

### 5. Error handling

Define specific error variants:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RmParseError {
    #[error("Invalid header: expected reMarkable .lines file")]
    InvalidHeader,
    #[error("Unsupported version: {0} (expected 6)")]
    UnsupportedVersion(u32),
    #[error("Unexpected end of file at offset {0}")]
    UnexpectedEof(usize),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
```

### 6. Convenience functions

```rust
/// Parse a .rm file from a file path
pub fn parse_rm_from_path(path: &Path) -> Result<RmPage, RmParseError>

/// Get the bounding box of all strokes in a page
impl RmPage {
    pub fn bounding_box(&self) -> (f32, f32, f32, f32)  // (min_x, min_y, max_x, max_y)
    pub fn is_empty(&self) -> bool                       // true if no strokes
    pub fn total_strokes(&self) -> usize
    pub fn total_points(&self) -> usize
}
```

## Files to Create/Modify

- `src/remarkable/rm_parser.rs` — full implementation
- `src/remarkable/mod.rs` — export the module

## Test Strategy

Since we may not have real .rm files during development, create test fixtures:

1. **Construct a minimal valid .rm v6 byte array in code** — build the header + 1 layer + 1 stroke + 3 points as raw bytes. Parse it. Verify all values roundtrip correctly.
2. **Invalid header** — feed garbage bytes, verify `InvalidHeader` error.
3. **Wrong version** — construct a valid header with version=5, verify `UnsupportedVersion` error.
4. **Empty page** — valid header, 0 layers. Should parse to `RmPage { layers: vec![] }`.
5. **Multiple layers and strokes** — construct 2 layers with different stroke counts, verify structure.
6. **Pen type mapping** — verify all known pen type integers map to correct enum variants.

If you can find a real `.rm` file from the `rmscene` test fixtures on GitHub (https://github.com/ricklupton/rmscene/tree/main/tests), download it and add it as a test fixture.

## Acceptance Criteria

1. `parse_rm_file` successfully parses a constructed v6 binary blob into `RmPage`.
2. All pen types and colors map correctly.
3. Point coordinates, pressure, speed, tilt all parse as expected.
4. Invalid inputs produce clear, typed errors.
5. All unit tests pass.
6. `cargo clippy` clean.
