#!/usr/bin/env python3
"""Rasterize high-detail 8-bit paletted RGB565 cover art for the display UI
into build/covers.bin at build time (zero heap allocations in display).

Layout of build/covers.bin:
    u8 count
    Per image:
        u16 width, u16 height, u32 offset
    At offset:
        u16 palette[256] (RGB565 little-endian)
        u8 pixels[width * height]
"""
import argparse, math, os, struct
from PIL import Image, ImageDraw, ImageFilter, ImageFont

REGULAR = [
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
]


def load_font(px):
    for p in REGULAR:
        if os.path.exists(p):
            return ImageFont.truetype(p, px)
    return ImageFont.load_default()


def lerp_color(c0, c1, t):
    return tuple(int(c0[i] + (c1[i] - c0[i]) * t) for i in range(3))


def vgrad(img, y0, y1, c0, c1):
    w = img.width
    px = img.load()
    span = max(1, y1 - y0)
    for y in range(max(0, y0), min(img.height, y1)):
        c = lerp_color(c0, c1, (y - y0) / span)
        for x in range(w):
            px[x, y] = c


def make_cube_cover(w=260, h=376):
    """Scenic mountain + sky + glowing 3D prism cover (Arceus-inspired)."""
    img = Image.new("RGB", (w, h), (20, 40, 70))
    # Sky gradient: warm cream-cyan top to soft horizon blue.
    vgrad(img, 0, 175, (230, 240, 242), (120, 176, 218))
    vgrad(img, 175, 275, (92, 140, 185), (42, 74, 96))
    vgrad(img, 275, h, (36, 62, 58), (18, 28, 34))

    draw = ImageDraw.Draw(img, "RGBA")

    # Soft clouds near the top.
    for cx, cy, rx, ry, col in [
        (48, 38, 62, 22, (255, 252, 245, 180)),
        (115, 26, 78, 26, (255, 250, 240, 165)),
        (210, 44, 68, 24, (245, 248, 255, 175)),
        (36, 98, 55, 18, (238, 244, 250, 130)),
        (225, 96, 64, 20, (238, 244, 250, 140)),
    ]:
        draw.ellipse((cx - rx, cy - ry, cx + rx, cy + ry), fill=col)

    # Distant mountain range.
    draw.polygon(
        [(0, 210), (42, 148), (92, 182), (140, 132), (198, 176), (260, 144), (260, 240), (0, 240)],
        fill=(88, 118, 152, 255),
    )

    # Main central volcano/mountain peak (warm sunlit rock + snow cap).
    draw.polygon(
        [(-10, 248), (74, 118), (118, 62), (146, 62), (192, 122), (270, 248)],
        fill=(142, 92, 86, 255),
    )
    # Shadowed right slope of the peak.
    draw.polygon(
        [(132, 62), (146, 62), (192, 122), (270, 248), (150, 248), (124, 140)],
        fill=(96, 68, 78, 255),
    )
    # Snow cap ridges.
    draw.polygon(
        [(118, 62), (146, 62), (170, 94), (148, 108), (132, 88), (114, 112), (96, 92)],
        fill=(246, 248, 252, 240),
    )

    # Mist layer at the mountain base.
    for my in range(215, 255, 4):
        a = int(110 * (1.0 - abs(my - 235) / 22.0))
        draw.rectangle((0, my, w, my + 4), fill=(210, 226, 238, a))

    # Rolling green foreground hills & winding river.
    draw.polygon([(0, 248), (110, 236), (260, 258), (260, h), (0, h)], fill=(48, 86, 68, 255))
    draw.polygon([(0, 272), (165, 254), (260, 278), (260, h), (0, h)], fill=(34, 66, 54, 255))
    # River reflection.
    draw.polygon(
        [(118, 246), (136, 246), (172, 316), (138, 316)],
        fill=(130, 188, 220, 190),
    )
    # Pine silhouettes and two explorer figures on the ridge.
    for tx in [18, 34, 52, 206, 224, 242]:
        ty = 262 + (tx % 17)
        draw.polygon([(tx, ty - 28), (tx - 11, ty), (tx + 11, ty)], fill=(20, 42, 36, 255))
        draw.polygon([(tx, ty - 18), (tx - 13, ty + 8), (tx + 13, ty + 8)], fill=(16, 34, 30, 255))

    # Foreground dark cliff silhouette at bottom.
    draw.polygon([(0, 312), (95, 296), (190, 308), (260, 298), (260, h), (0, h)], fill=(22, 30, 34, 255))
    # Two small adventurer silhouettes with red/blue scarves.
    draw.ellipse((78, 272, 88, 282), fill=(30, 24, 28, 255))
    draw.polygon([(75, 282), (91, 282), (94, 306), (72, 306)], fill=(180, 42, 48, 255))
    draw.ellipse((142, 278, 151, 287), fill=(30, 24, 28, 255))
    draw.polygon([(139, 287), (154, 287), (156, 310), (137, 310)], fill=(42, 88, 165, 255))

    # Center Emblem: Golden / Cyan ring + 3D isometric Cube + "CUBE / LEGENDS / NXK" logo!
    cx, cy = w // 2, 166
    draw.arc((cx - 72, cy - 64, cx + 72, cy + 64), 195, 345, fill=(248, 212, 104, 235), width=4)
    draw.arc((cx - 66, cy - 58, cx + 66, cy + 58), 15, 165, fill=(248, 212, 104, 210), width=3)

    # 3D isometric glowing cube above title.
    utop = [(cx, cy - 62), (cx + 28, cy - 46), (cx, cy - 30), (cx - 28, cy - 46)]
    uleft = [(cx - 28, cy - 46), (cx, cy - 30), (cx, cy + 2), (cx - 28, cy - 14)]
    uright = [(cx, cy - 30), (cx + 28, cy - 46), (cx + 28, cy - 14), (cx, cy + 2)]
    draw.polygon(utop, fill=(110, 220, 255, 245))
    draw.polygon(uleft, fill=(36, 152, 235, 245))
    draw.polygon(uright, fill=(20, 96, 185, 245))
    draw.line(utop + [utop[0]], fill=(255, 255, 255, 240), width=2)

    f_main = load_font(34)
    f_sub = load_font(13)
    f_bot = load_font(26)

    # Outlined "CUBE" banner.
    for dx, dy in [(-2, 0), (2, 0), (0, -2), (0, 2), (-2, -2), (2, 2)]:
        draw.text((cx + dx, cy - 10 + dy), "CUBE", font=f_main, fill=(16, 48, 102, 255), anchor="mm")
    draw.text((cx, cy - 10), "CUBE", font=f_main, fill=(255, 232, 92, 255), anchor="mm")

    # Pill "LEGENDS"
    draw.rounded_rectangle((cx - 48, cy + 10, cx + 48, cy + 28), radius=8, fill=(22, 138, 220, 255), outline=(255, 255, 255, 230), width=2)
    draw.text((cx, cy + 19), "LEGENDS", font=f_sub, fill=(255, 255, 255, 255), anchor="mm")

    # Dark metallic "ARCEUS / GM20B" style bottom title
    for dx, dy in [(-2, 0), (2, 0), (0, -2), (0, 2)]:
        draw.text((cx + dx, cy + 48 + dy), "HYPERPRISM", font=f_bot, fill=(14, 20, 32, 255), anchor="mm")
    draw.text((cx, cy + 48), "HYPERPRISM", font=f_bot, fill=(238, 244, 252, 255), anchor="mm")

    return img


def make_starlane_cover(w=228, h=318):
    """Stacked card cover: top 'MINECRAFT' strip + bottom 'HOLLOW KNIGHT / STARLANE' dark cover."""
    img = Image.new("RGB", (w, h), (16, 18, 24))
    draw = ImageDraw.Draw(img, "RGBA")

    # Top strip (y = 0..56): blocky sky & terrain banner with bold title.
    vgrad(img, 0, 56, (78, 152, 224), (58, 118, 72))
    for bx in range(0, w, 18):
        draw.rectangle((bx, 38, bx + 17, 56), fill=(96, 68, 44, 220))
        draw.rectangle((bx, 34, bx + 17, 40), fill=(76, 168, 64, 240))
    f_top = load_font(22)
    for dx, dy in [(-2, 0), (2, 0), (0, -2), (0, 2)]:
        draw.text((w // 2 + dx, 26 + dy), "MINECRAFT", font=f_top, fill=(24, 24, 28, 255), anchor="mm")
    draw.text((w // 2, 26), "MINECRAFT", font=f_top, fill=(224, 230, 238, 255), anchor="mm")
    draw.line((0, 56, w, 56), fill=(10, 12, 16, 255), width=2)

    # Bottom main cover (y = 57..h): deep midnight blue Starlane / Hollow Knight style art.
    vgrad(img, 57, 210, (12, 16, 30), (28, 42, 74))
    vgrad(img, 210, h, (28, 42, 74), (10, 12, 20))

    # Glowing moon/nebula behind title.
    cx = w // 2
    for r, a in [(76, 24), (56, 42), (38, 68)]:
        draw.ellipse((cx - r, 165 - r, cx + r, 165 + r), fill=(140, 195, 255, a))

    # Star particles.
    for sx, sy, sr in [(28, 82, 2), (52, 118, 1), (188, 78, 2), (204, 124, 1), (42, 174, 2), (195, 182, 2), (82, 74, 1), (154, 92, 2)]:
        draw.ellipse((sx - sr, sy - sr, sx + sr, sy + sr), fill=(220, 240, 255, 220))

    # Ornate crest lines around the title.
    draw.line((24, 98, w - 24, 98), fill=(185, 205, 230, 180), width=1)
    draw.line((36, 146, w - 36, 146), fill=(185, 205, 230, 180), width=1)
    f_k = load_font(21)
    for dx, dy in [(-1, 0), (1, 0), (0, -1), (0, 1)]:
        draw.text((cx + dx, 122 + dy), "STARLANE", font=f_k, fill=(8, 12, 22, 255), anchor="mm")
    draw.text((cx, 122), "STARLANE", font=f_k, fill=(242, 246, 255, 255), anchor="mm")

    # Neon perspective grid at the bottom.
    hy = 238
    for i in range(-4, 5):
        draw.line((cx + i * 10, hy, cx + i * 38, h), fill=(56, 180, 255, 95), width=1)
    for y in [250, 266, 286, 308]:
        draw.line((0, y, w, y), fill=(56, 180, 255, 85), width=1)

    # Stylized white horned mask / starfighter figure in the center-bottom.
    # Cloak body
    draw.polygon([(cx - 32, h), (cx - 18, 236), (cx + 18, 236), (cx + 32, h)], fill=(38, 48, 68, 255))
    # White horned head/hull
    draw.polygon([(cx - 22, 214), (cx - 28, 172), (cx - 10, 194), (cx + 10, 194), (cx + 28, 172), (cx + 22, 214)], fill=(236, 240, 246, 255))
    draw.ellipse((cx - 22, 190, cx + 22, 238), fill=(236, 240, 246, 255))
    # Dark eyes
    draw.ellipse((cx - 14, 206, cx - 4, 222), fill=(14, 18, 26, 255))
    draw.ellipse((cx + 4, 206, cx + 14, 222), fill=(14, 18, 26, 255))

    return img


def make_starlane_full(w=260, h=376):
    """Full portrait cover when Starlane is the selected card (260x376)."""
    img = Image.new("RGB", (w, h), (12, 16, 28))
    vgrad(img, 0, 230, (10, 14, 28), (32, 52, 92))
    vgrad(img, 230, h, (32, 52, 92), (10, 12, 22))
    draw = ImageDraw.Draw(img, "RGBA")
    cx = w // 2
    for r, a in [(96, 26), (72, 46), (48, 74)]:
        draw.ellipse((cx - r, 175 - r, cx + r, 175 + r), fill=(120, 195, 255, a))
    for sx, sy, sr in [(28, 42, 2), (62, 68, 1), (208, 48, 2), (224, 94, 2), (42, 144, 2), (215, 162, 2), (92, 44, 1), (164, 62, 2)]:
        draw.ellipse((sx - sr, sy - sr, sx + sr, sy + sr), fill=(225, 242, 255, 230))
    draw.line((26, 86, w - 26, 86), fill=(195, 215, 240, 190), width=2)
    draw.line((40, 142, w - 40, 142), fill=(195, 215, 240, 190), width=2)
    f_main = load_font(30)
    f_sub = load_font(13)
    for dx, dy in [(-2, 0), (2, 0), (0, -2), (0, 2)]:
        draw.text((cx + dx, 114 + dy), "STARLANE", font=f_main, fill=(8, 12, 24, 255), anchor="mm")
    draw.text((cx, 114), "STARLANE", font=f_main, fill=(245, 248, 255, 255), anchor="mm")
    draw.text((cx, 158), "NEON ODYSSEY", font=f_sub, fill=(130, 210, 255, 240), anchor="mm")

    hy = 248
    for i in range(-5, 6):
        draw.line((cx + i * 10, hy, cx + i * 44, h), fill=(46, 210, 255, 115), width=2)
    for y in [262, 280, 302, 328, 356]:
        draw.line((0, y, w, y), fill=(46, 210, 255, 95), width=1)

    # Sleek starfighter over the neon grid.
    draw.polygon([(cx, 196), (cx - 48, 276), (cx - 14, 264), (cx, 282), (cx + 14, 264), (cx + 48, 276)], fill=(235, 242, 252, 255))
    draw.polygon([(cx, 206), (cx - 14, 256), (cx + 14, 256)], fill=(36, 175, 255, 245))
    draw.ellipse((cx - 18, 262, cx - 6, 274), fill=(255, 180, 60, 255))
    draw.ellipse((cx + 6, 262, cx + 18, 274), fill=(255, 180, 60, 255))
    return img


def make_left_peek(w=64, h=352):
    """Left-edge peeking card ('...HTER' green/gold/sky artwork from the reference)."""
    img = Image.new("RGB", (w, h), (28, 42, 68))
    vgrad(img, 0, 120, (130, 205, 242), (236, 224, 188))
    vgrad(img, 120, 260, (52, 168, 82), (24, 98, 48))
    vgrad(img, 260, h, (28, 32, 38), (18, 20, 24))
    draw = ImageDraw.Draw(img, "RGBA")
    # Diagonal paper-cut shapes like the reference image's left card.
    draw.polygon([(0, 48), (w, 18), (w, 96), (0, 118)], fill=(228, 216, 192, 255))
    draw.polygon([(0, 118), (w, 96), (w, 224), (0, 242)], fill=(38, 176, 78, 255))
    draw.ellipse((-24, 134, 48, 212), fill=(248, 206, 48, 255))
    # Bottom red strip & bold "...HTER" text
    draw.rectangle((0, h - 38, w, h - 22), fill=(214, 36, 36, 255))
    f = load_font(22)
    draw.text((w - 4, h - 62), "HTER", font=f, fill=(255, 255, 255, 255), anchor="rm")
    return img


def pack_rgb565_paletted(img):
    """Quantize RGB image to 256 colors and pack (palette_u16[256], indices_u8)."""
    q = img.quantize(colors=256, method=Image.Quantize.MEDIANCUT)
    raw_pal = q.getpalette() or []
    raw_pal += [0] * (768 - len(raw_pal))
    pal_u16 = []
    for i in range(256):
        r, g, b = raw_pal[3 * i], raw_pal[3 * i + 1], raw_pal[3 * i + 2]
        c565 = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)
        pal_u16.append(c565)
    pal_bytes = struct.pack("<256H", *pal_u16)
    idx_bytes = q.tobytes()
    return img.width, img.height, pal_bytes + idx_bytes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", required=True)
    a = ap.parse_args()

    imgs = [
        make_cube_cover(260, 376),
        make_starlane_full(260, 376),
        make_starlane_cover(228, 318),
        make_left_peek(64, 352),
    ]
    packed = [pack_rgb565_paletted(im) for im in imgs]
    head = bytearray([len(packed)])
    off = 1 + 8 * len(packed)
    table = bytearray()
    blobs = []
    for w, h, data in packed:
        table += struct.pack("<HHI", w, h, off)
        blobs.append(data)
        off += len(data)

    os.makedirs(os.path.dirname(os.path.abspath(a.o)), exist_ok=True)
    with open(a.o, "wb") as f:
        f.write(bytes(head) + bytes(table) + b"".join(blobs))
    print(f"mkcovers: {a.o}: {off} bytes ({len(packed)} images)")


if __name__ == "__main__":
    main()
