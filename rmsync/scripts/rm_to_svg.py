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
VIEW_W, VIEW_H = 1404, 1872
HIGHLIGHT_TOOLS = {18}  # Highlighterv2


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

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" '
        f'viewBox="0 0 {VIEW_W} {VIEW_H}" '
        f'width="{VIEW_W}" height="{VIEW_H}">',
        f'<rect width="{VIEW_W}" height="{VIEW_H}" fill="white"/>',
    ]

    for stroke in strokes:
        pts = stroke.points
        if len(pts) < 2:
            continue
        color = TOOL_COLORS.get(stroke.color, "black")
        # Use per-point width via variable-width polyline
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
    print(f"OK {len(strokes)} strokes -> {svg_path}")


if __name__ == "__main__":
    main()
