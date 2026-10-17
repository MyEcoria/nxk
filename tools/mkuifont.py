#!/usr/bin/env python3
"""Antialiased sans-serif of the display UI, rasterized at build time into
build/uifont.bin (nothing vendored, like Terminus for the terminal).

Defaults to the first of Lato (SIL OFL 1.1, package fonts-lato: closest
to the Switch's menus), Noto Sans (OFL), Liberation Sans (OFL) or DejaVu
Sans (Bitstream Vera license) found on the PC, in three weights (regular,
semibold, bold; a missing one falls back to the next heavier, then to
regular). `--ttf`, `--semibold`, `--bold` or `NXK_UI_FONT`,
`NXK_UI_FONT_SEMIBOLD`, `NXK_UI_FONT_BOLD` pick others. Without any,
plain boxes (the build still works, the screen reads badly).

    mkuifont.py -o build/uifont.bin [--ttf FILE] [--semibold FILE] [--bold FILE]

File (little endian), read by user/display/src/font.rs:
    u8 sizes, then per size 8 bytes: u8 line height, u8 ascent, u8 cap
    height, u8 0, u32 offset of its block; each block is 101 glyph records of
    12 bytes (u8 w, u8 h, i8 x offset, i8 rows above the baseline, u8
    advance, 3 x 0, u32 offset in the block's bitmap) then the bitmaps:
    4-bit alpha, row by row, the high nibble first.
"""
import argparse, os, struct, sys

from PIL import Image, ImageDraw, ImageFont

# One entry per `font::Face`: pixel size, weight, characters drawn (None: all).
FACES = [
    (18, "regular", None),
    (18, "semibold", None),
    (21, "regular", None),
    (26, "regular", None),
    (26, "semibold", None),
    (31, "regular", None),
    (30, "bold", None),
    (48, "bold", None),
    (64, "bold", "0123456789.:%-\u00b0 /ABCDEFGHIJKLMNOPQRSTUVWXYZibs"),
]
# Glyph slots: ASCII 32..=126, then these (font.rs `slot`).
EXTRA = "\u00b0\u00b7\u2022\u00d7\u2039\u203a"
CHARS = [chr(c) for c in range(32, 127)] + list(EXTRA)

REGULAR = [
    "/usr/share/fonts/truetype/lato/Lato-Regular.ttf",
    "/usr/share/fonts/lato/Lato-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
]
BOLD = [p.replace("Regular", "Bold").replace("DejaVuSans.ttf", "DejaVuSans-Bold.ttf") for p in REGULAR]
# Only Lato has the weight; elsewhere `first` moves on to the bold.
SEMIBOLD = [p.replace("Regular", "Semibold") for p in REGULAR if "Lato" in p]


def first(paths):
    return next((p for p in paths if p and os.path.exists(p)), None)


def render(font, px, only):
    """Records and bitmaps of one face."""
    asc, desc = font.getmetrics() if font else (px, px // 4)
    cap = font.getbbox("H", anchor="ls")[1] if font else -(px * 7 // 10)
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
    return asc + desc, asc, -cap, b"".join(recs) + bytes(bits)


ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
ap.add_argument("--ttf", default=os.environ.get("NXK_UI_FONT"))
ap.add_argument("--semibold", default=os.environ.get("NXK_UI_FONT_SEMIBOLD"))
ap.add_argument("--bold", default=os.environ.get("NXK_UI_FONT_BOLD"))
a = ap.parse_args()
# A font given by hand keeps its own family: no Lato weights mixed in.
reg = a.ttf or first(REGULAR)
bold = a.bold or (None if a.ttf else first(BOLD)) or reg
semi = a.semibold or (None if a.ttf else first(SEMIBOLD)) or bold
paths = {"regular": reg, "semibold": semi, "bold": bold}
if not reg:
    print("mkuifont: no sans-serif font found: boxes only", file=sys.stderr)

blocks, head = [], bytearray([len(FACES)])
off = 1 + 8 * len(FACES)
table = bytearray()
for px, weight, only in FACES:
    path = paths[weight]
    font = ImageFont.truetype(path, px) if path else None
    line, asc, cap, block = render(font, px, only)
    table += struct.pack("<BBBBI", line, asc, cap, 0, off)
    blocks.append(block)
    off += len(block)
os.makedirs(os.path.dirname(os.path.abspath(a.o)), exist_ok=True)
with open(a.o, "wb") as f:
    f.write(bytes(head) + bytes(table) + b"".join(blocks))
print(f"mkuifont: {a.o}: {off} bytes ({os.path.basename(reg) if reg else 'boxes'})")
