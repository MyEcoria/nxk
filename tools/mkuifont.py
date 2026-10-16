#!/usr/bin/env python3
"""Antialiased sans-serif of the display UI, rasterized at build time into
build/uifont.bin (nothing vendored, like Terminus for the terminal).

Defaults to the first of Noto Sans (SIL OFL 1.1), Liberation Sans (OFL)
or DejaVu Sans (Bitstream Vera license) found on the PC; `--ttf` and
`--bold` pick others, `NXK_UI_FONT` / `NXK_UI_FONT_BOLD` too. Without any,
plain boxes (the build still works, the screen reads badly).

    mkuifont.py -o build/uifont.bin [--ttf FILE] [--bold FILE]

File (little endian), read by user/display/src/font.rs:
    u8 sizes, then per size 8 bytes: u8 line height, u8 ascent, u8 0,
    u8 0, u32 offset of its block; each block is 101 glyph records of
    12 bytes (u8 w, u8 h, i8 x offset, i8 rows above the baseline, u8
    advance, 3 x 0, u32 offset in the block's bitmap) then the bitmaps:
    4-bit alpha, row by row, the high nibble first.
"""
import argparse, os, struct, sys

from PIL import Image, ImageDraw, ImageFont

# One entry per `font::Face`: pixel size, bold, characters drawn (None: all).
FACES = [
    (15, False, None),
    (20, False, None),
    (20, True, None),
    (26, False, None),
    (26, True, None),
    (38, True, None),
    (64, True, "0123456789.:%-\u00b0 /ABCDEFGHIJKLMNOPQRSTUVWXYZibs"),
]
# Glyph slots: ASCII 32..=126, then these (font.rs `slot`).
EXTRA = "\u00b0\u00b7\u2022\u00d7\u2039\u203a"
CHARS = [chr(c) for c in range(32, 127)] + list(EXTRA)

REGULAR = [
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
]
BOLD = [p.replace("Regular", "Bold").replace("DejaVuSans.ttf", "DejaVuSans-Bold.ttf") for p in REGULAR]


def first(paths):
    return next((p for p in paths if p and os.path.exists(p)), None)


def render(font, px, only):
    """Records and bitmaps of one face."""
    asc, desc = font.getmetrics()
    base = 2 * px
    recs, bits = [], bytearray()
    for ch in CHARS:
        adv = int(round(font.getlength(ch))) if font else px // 2
        w = h = xo = yo = 0
        data = b""
        if font and (only is None or ch in only) and ch != " ":
            img = Image.new("L", (3 * px, 3 * px), 0)
            ImageDraw.Draw(img).text((px, base), ch, font=font, fill=255, anchor="ls")
            bb = img.getbbox()
            if bb:
                l, t, r, b = bb
                w, h, xo, yo = r - l, b - t, l - px, base - t
                cut = img.crop(bb)
                px_ = list(cut.tobytes())
                packed = bytearray()
                for i in range(0, len(px_), 2):
                    hi = px_[i] >> 4
                    lo = (px_[i + 1] >> 4) if i + 1 < len(px_) else 0
                    packed.append(hi << 4 | lo)
                data = bytes(packed)
        elif not font and ch != " " and (only is None or ch in only):
            w, h, yo = max(px // 3, 1), max(px * 2 // 3, 1), max(px * 2 // 3, 1)
            data = bytes([0xFF] * ((w * h + 1) // 2))
        assert w < 256 and h < 256 and -128 <= xo < 128 and -128 <= yo < 128
        recs.append(struct.pack("<BBbbB3xI", w, h, xo, yo, min(adv, 255), len(bits)))
        bits += data
    return asc + desc, asc, b"".join(recs) + bytes(bits)


ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
ap.add_argument("--ttf", default=os.environ.get("NXK_UI_FONT"))
ap.add_argument("--bold", default=os.environ.get("NXK_UI_FONT_BOLD"))
a = ap.parse_args()
reg = a.ttf or first(REGULAR)
bold = a.bold or first(BOLD) or reg
if not reg:
    print("mkuifont: no sans-serif font found: boxes only", file=sys.stderr)

blocks, head = [], bytearray([len(FACES)])
off = 1 + 8 * len(FACES)
table = bytearray()
for px, is_bold, only in FACES:
    path = bold if is_bold else reg
    font = ImageFont.truetype(path, px) if path else None
    line, asc, block = render(font, px, only)
    table += struct.pack("<BBBBI", line, asc, 0, 0, off)
    blocks.append(block)
    off += len(block)
os.makedirs(os.path.dirname(os.path.abspath(a.o)), exist_ok=True)
with open(a.o, "wb") as f:
    f.write(bytes(head) + bytes(table) + b"".join(blocks))
print(f"mkuifont: {a.o}: {off} bytes ({os.path.basename(reg) if reg else 'boxes'})")
