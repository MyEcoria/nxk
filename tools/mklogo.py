#!/usr/bin/env python3
"""Generate the nxk pixel-art logo.

    mklogo.py [DIR]          # SVGs, default docs/logo/
    mklogo.py --raw FILE     # boot screen image for the display service

The icon draws the microkernel as rings: the EL1 core at the center, driver
arcs around it, then isolated EL0 service arcs. Each SVG comes in a `dark`
variant (for dark backgrounds) and a `light` one. The raw image is: width,
height (1 byte each), 8 palette entries as R G B A bytes (index 0 unused:
transparent), then one palette index per pixel, row by row.
"""
import sys
from pathlib import Path

N = 25  # icon grid
C = N // 2

# 1 shadow, 2 dark rust, 3 rust, 4 orange, 5 light orange, 6 cream, 7 core
PAL_DARK = {1: "#3b1a0c", 2: "#8a3413", 3: "#c2491a", 4: "#e8691f", 5: "#f7a14a", 6: "#ffd9a0", 7: "#fff4e0"}
PAL_LIGHT = {1: "#2a1208", 2: "#6e2710", 3: "#a33a12", 4: "#cf5a18", 5: "#e8801f", 6: "#f7a14a", 7: "#ffe2b0"}


def icon():
    g = [[0] * N for _ in range(N)]
    for y in range(N):
        for x in range(N):
            dx, dy = x - C, y - C
            d2, ax, ay = dx * dx + dy * dy, abs(dx), abs(dy)
            if 110 < d2 <= 156:  # service arcs, split on the axes
                if ax > 1 and ay > 1:
                    g[y][x] = 4 if dx < 0 and dy < 0 else 2 if dx > 0 and dy > 0 else 3
            elif 42 < d2 <= 72:  # driver arcs, split on the diagonals
                if abs(ax - ay) > 1:
                    g[y][x] = 5 if (ay > ax and dy < 0) or (ax > ay and dx < 0) else 4
            elif d2 <= 10:  # core, lit from the top-left
                g[y][x] = 7 if d2 <= 2 else 6 if dx + dy <= 0 else 5
    return g


# 2-pixel stroke font: x-height 11, 4-row ascender.
XH, ASC = 11, 4
GLYPHS = {
    "n": [
        "##.#####..",
        "#########.",
        "###....###",
        *["##......##"] * 8,
    ],
    "x": [
        "##......##",
        "###....###",
        ".###..###.",
        "..######..",
        *["...####..."] * 3,
        "..######..",
        ".###..###.",
        "###....###",
        "##......##",
    ],
    "k": [
        *["##......."] * 4,
        "##.....##",
        "##....###",
        "##...###.",
        "##..###..",
        "##.###...",
        "#####....",
        "######...",
        "###.###..",
        "##...###.",
        "##....###",
        "##.....##",
    ],
}


def wordmark(text="nxk", gap=2):
    h = XH + ASC
    cols = []
    for i, ch in enumerate(text):
        rows = ["." * len(GLYPHS[ch][0])] * (h - len(GLYPHS[ch])) + GLYPHS[ch]
        cols += [[r[c] == "#" for r in rows] for c in range(len(rows[0]))]
        if i < len(text) - 1:
            cols += [[False] * h] * gap
    w = len(cols)
    ink = lambda x, y: 0 <= x < w and 0 <= y < h and cols[x][y]
    # title-screen color bands, with a 1-pixel drop shadow to the bottom-right
    band = lambda y: 6 if y < ASC + 2 else 5 if y < ASC + 5 else 4 if y < ASC + 9 else 3
    g = [[0] * (w + 1) for _ in range(h + 1)]
    for y in range(h + 1):
        for x in range(w + 1):
            if ink(x, y):
                g[y][x] = band(y)
            elif ink(x - 1, y - 1) or ink(x, y - 1) or ink(x - 1, y):
                g[y][x] = 1
    return g


def compose(parts, gap):
    """Place grids side by side, vertically centered."""
    h = max(len(p) for p in parts)
    g = [[0] * (sum(len(p[0]) for p in parts) + gap * (len(parts) - 1)) for _ in range(h)]
    x0 = 0
    for p in parts:
        y0 = (h - len(p)) // 2
        for y, row in enumerate(p):
            for x, v in enumerate(row):
                if v:
                    g[y0 + y][x0 + x] = v
        x0 += len(p[0]) + gap
    return g


def svg(g, pal, scale=8):
    """One path per color, one subpath per horizontal run of pixels."""
    h, w = len(g), len(g[0])
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" '
        f'width="{w * scale}" height="{h * scale}" shape-rendering="crispEdges">',
        "<title>nxk</title>",
    ]
    for v in sorted(pal):
        runs = []
        for y, row in enumerate(g):
            x = 0
            while x < w:
                x1 = x
                while x1 < w and row[x1] == v:
                    x1 += 1
                if x1 > x:
                    runs.append(f"M{x} {y}h{x1 - x}v1h{x - x1}z")
                x = max(x1, x + 1)
        if runs:
            out.append(f'<path fill="{pal[v]}" d="{"".join(runs)}"/>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def raw(g, pal):
    out = bytearray([len(g[0]), len(g)])
    for i in range(8):
        out += bytes.fromhex(pal[i][1:]) + b"\xff" if i in pal else bytes(4)
    for row in g:
        out += bytes(row)
    return bytes(out)


def main():
    ic = icon()
    logo = compose([ic, wordmark()], gap=5)
    if sys.argv[1:2] == ["--raw"]:
        Path(sys.argv[2]).write_bytes(raw(logo, PAL_DARK))
        return
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent / "docs" / "logo"
    out.mkdir(parents=True, exist_ok=True)
    for name, g in (("nxk-icon", ic), ("nxk-logo", logo)):
        for theme, pal in (("dark", PAL_DARK), ("light", PAL_LIGHT)):
            (out / f"{name}-{theme}.svg").write_text(svg(g, pal))
            print(out / f"{name}-{theme}.svg")


if __name__ == "__main__":
    main()
