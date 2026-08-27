#!/usr/bin/env python3
"""Convert a reMarkable .rm v6 scene-tree file to SVG.

Uses the `rmscene` library to parse the CRDT-based block format used by
firmware 3.x+. Called by rmsync's viewer when the Rust-native v6 flat
parser fails (the flat parser handles legacy v6 files that pre-date the
scene-tree format).

Usage:
    rm_to_svg.py <input.rm> <output.svg>

Install dependency:
    pip install rmscene
"""
import sys
from pathlib import Path

TOOL_COLORS = {0: "black", 1: "gray", 2: "white"}
HIGHLIGHT_OPACITY = 0.35
HIGHLIGHT_TOOLS = {18}  # Highlighterv2
PADDING = 20  # px padding around content bounding box


def main():
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} <input.rm> <output.svg>", file=sys.stderr)
        sys.exit(1)

    rm_path = Path(sys.argv[1])
    svg_path = Path(sys.argv[2])

    try:
        from rmscene import read_blocks, SceneLineItemBlock
    except ImportError:
        print("ERROR: rmscene not installed. Run: pip install rmscene",
              file=sys.stderr)
        sys.exit(2)

    with open(rm_path, "rb") as f:
        blocks = list(read_blocks(f))

    strokes = [
        b.item.value
        for b in blocks
        if isinstance(b, SceneLineItemBlock) and b.item.value is not None
    ]

    if not strokes:
        # Empty page — write a blank SVG at the default viewport.
        svg_path.write_text(
            '<svg xmlns="http://www.w3.org/2000/svg" '
            'viewBox="0 0 1404 1872" width="1404" height="1872">'
            '<rect width="1404" height="1872" fill="white"/>'
            '</svg>'
        )
        print(f"OK 0 strokes (blank page) -> {svg_path}")
        return

    # --- Compute the bounding box of all stroke points ---
    min_x = float("inf")
    min_y = float("inf")
    max_x = float("-inf")
    max_y = float("-inf")
    for stroke in strokes:
        for p in stroke.points:
            # Account for stroke width so thick lines aren't clipped
            hw = p.width * 0.4 * 0.5  # half of rendered stroke width
            if p.x - hw < min_x:
                min_x = p.x - hw
            if p.y - hw < min_y:
                min_y = p.y - hw
            if p.x + hw > max_x:
                max_x = p.x + hw
            if p.y + hw > max_y:
                max_y = p.y + hw

    # Add padding
    min_x -= PADDING
    min_y -= PADDING
    max_x += PADDING
    max_y += PADDING

    vb_w = max_x - min_x
    vb_h = max_y - min_y

    # Use the bounding-box dimensions as the SVG viewport so all content
    # is visible and fills the width. The viewer's resvg pipeline scales
    # the result to fit the panel.
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" '
        f'viewBox="{min_x:.2f} {min_y:.2f} {vb_w:.2f} {vb_h:.2f}" '
        f'width="{vb_w:.0f}" height="{vb_h:.0f}">',
        f'<rect x="{min_x:.2f}" y="{min_y:.2f}" '
        f'width="{vb_w:.2f}" height="{vb_h:.2f}" fill="white"/>',
    ]

    for stroke in strokes:
        pts = stroke.points
        if len(pts) < 2:
            continue
        color = TOOL_COLORS.get(stroke.color, "black")
        opacity = HIGHLIGHT_OPACITY if stroke.tool in HIGHLIGHT_TOOLS else 1.0
        avg_width = sum(p.width for p in pts) / len(pts) * 0.4
        d = f"M {pts[0].x:.2f} {pts[0].y:.2f}"
        for p in pts[1:]:
            d += f" L {p.x:.2f} {p.y:.2f}"
        parts.append(
            f'<path d="{d}" stroke="{color}" '
            f'stroke-width="{avg_width:.2f}" fill="none" '
            f'stroke-linecap="round" stroke-linejoin="round"'
            + (f' opacity="{opacity}"' if opacity < 1.0 else "")
            + "/>"
        )

    parts.append("</svg>")
    svg_path.write_text("\n".join(parts))
    print(f"OK {len(strokes)} strokes, "
          f"bbox=({min_x:.0f},{min_y:.0f})..({max_x:.0f},{max_y:.0f}) "
          f"-> {svg_path}")


if __name__ == "__main__":
    main()
