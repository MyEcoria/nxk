#!/usr/bin/env python3
"""Art of the display UI, drawn at build time (nothing vendored).

    mkcovers.py -o build/uiart.bin --games build/covers

`-o`: the UI's own images, built into display: the player avatar
(64x64), the home screen's round buttons (System, Network, Terminal,
Logs, Controllers, System Settings, Power; 76x76, on the dark theme's disc, then on
the light one's) and the same glyphs for the page headers (40x40, on
each theme's background). `--games DIR`: one DIR/NAME.bin per game of
jeux/ (title and color from its game.txt), installed by tools/mksd.py as
/games/NAME/cover.bin: its square icon (256x256). A game without a
drawing of its own gets its title on its color.

File (little endian), read by user/display/src/art.rs:
    u8 count, then per image: u16 width, u16 height, u32 offset; at the
    offset: u16 palette[256] (RGB565), then one index per pixel, row by
    row.
"""
import argparse, math, os, random, struct, sys

from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mklogo  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# A game's icon; the avatar, the home's round buttons, the header glyphs.
ICON = 256
AVATAR, BUTTON, HEAD = 64, 76, 40
# Drawn this many times larger, then filtered down.
SS = 3

FONTS = {
    "black": ["/usr/share/fonts/truetype/lato/Lato-Black.ttf"],
    "blackitalic": ["/usr/share/fonts/truetype/lato/Lato-BlackItalic.ttf"],
    "bold": [
        "/usr/share/fonts/truetype/lato/Lato-Bold.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ],
}


def font(kind, px):
    for p in FONTS[kind] + FONTS["bold"]:
        if os.path.exists(p):
            return ImageFont.truetype(p, px)
    return ImageFont.load_default()


# Drawing helpers.

def gradient(w, h, stops, horizontal=False):
    """Linear gradient through `stops` [(t, (r, g, b)), ...]."""
    n = 512
    strip = []
    for i in range(n):
        t = i / (n - 1)
        for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
            if t0 <= t <= t1:
                k = (t - t0) / max(t1 - t0, 1e-6)
                strip.append(tuple(int(a + (b - a) * k) for a, b in zip(c0, c1)))
                break
        else:
            strip.append(stops[-1][1])
    img = Image.new("RGB", (n, 1) if horizontal else (1, n))
    img.putdata(strip)
    return img.resize((w, h), Image.BILINEAR)


def glow(base, layer, radius, strength=1.0):
    """`layer` (RGBA) added to `base` blurred, then sharp on top."""
    blur = layer.filter(ImageFilter.GaussianBlur(radius))
    if strength != 1.0:
        a = blur.getchannel("A").point(lambda v: min(255, int(v * strength)))
        blur.putalpha(a)
    base.alpha_composite(blur)
    base.alpha_composite(layer)


def layer(img):
    return Image.new("RGBA", img.size, (0, 0, 0, 0))


def ridge(rng, w, y, rough, steps=7):
    """Mountain line across `w`: midpoint displacement around `y`."""
    pts = [(0, y + rng.uniform(-rough, rough)), (w, y + rng.uniform(-rough, rough))]
    for _ in range(steps):
        out = []
        for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
            out.append((x0, y0))
            out.append(((x0 + x1) / 2, (y0 + y1) / 2 + rng.uniform(-rough, rough)))
        out.append(pts[-1])
        pts = out
        rough *= 0.55
    return pts


def logo(img, text, cx, cy, px, fill_stops, stroke, kind="black", glow_c=None, shadow=True):
    """Title in the key art style: gradient fill, thick outline, shadow."""
    f = font(kind, px)
    sw = max(2, px // 9)
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).text((cx, cy), text, font=f, fill=255, anchor="mm")
    outline = Image.new("L", img.size, 0)
    ImageDraw.Draw(outline).text(
        (cx, cy), text, font=f, fill=255, anchor="mm", stroke_width=sw, stroke_fill=255
    )
    if shadow:
        sh = Image.new("RGBA", img.size, (0, 0, 0, 0))
        sh.putalpha(ImageChops.offset(outline, 0, sw).filter(ImageFilter.GaussianBlur(sw)))
        img.alpha_composite(sh)
    if glow_c:
        g = Image.new("RGBA", img.size, glow_c + (0,))
        g.putalpha(outline.filter(ImageFilter.GaussianBlur(sw * 3)))
        img.alpha_composite(g)
        img.alpha_composite(g)
    o = Image.new("RGBA", img.size, stroke + (255,))
    o.putalpha(outline)
    img.alpha_composite(o)
    x0, y0, x1, y1 = mask.getbbox()
    fill = Image.new("RGBA", img.size, (0, 0, 0, 0))
    fill.paste(gradient(x1 - x0, y1 - y0, fill_stops).convert("RGBA"), (x0, y0))
    fill.putalpha(ImageChops.multiply(fill.getchannel("A"), mask))
    img.alpha_composite(fill)
    # Gloss on the upper half of the letters.
    hi = Image.new("RGBA", img.size, (255, 255, 255, 0))
    band = Image.new("L", img.size, 0)
    ImageDraw.Draw(band).rectangle((x0, y0, x1, y0 + (y1 - y0) * 0.42), fill=70)
    hi.putalpha(ImageChops.multiply(band, mask))
    img.alpha_composite(hi)
    return y1 - y0


def shade(c, k):
    return tuple(max(0, min(255, int(v * k))) for v in c)


def lighten(c, k):
    return tuple(int(v + (255 - v) * k) for v in c)


def project(p, cx, cy, s, d=6.0):
    x, y, z = p
    f = s * d / (d + z)
    return (cx + x * f, cy - y * f)


def rot(p, yaw, pitch):
    x, y, z = p
    cyw, syw = math.cos(yaw), math.sin(yaw)
    x, z = x * cyw + z * syw, -x * syw + z * cyw
    cp, sp = math.cos(pitch), math.sin(pitch)
    y, z = y * cp - z * sp, y * sp + z * cp
    return (x, y, z)


def cube3d(img, cx, cy, s, yaw, pitch, colors, light=(-0.5, 0.8, -0.6), edge=255):
    """A lit cube; `colors`: +x, -x, +y, -y, +z, -z faces."""
    faces = [
        ((1, 0, 0), [(1, -1, -1), (1, 1, -1), (1, 1, 1), (1, -1, 1)]),
        ((-1, 0, 0), [(-1, -1, 1), (-1, 1, 1), (-1, 1, -1), (-1, -1, -1)]),
        ((0, 1, 0), [(-1, 1, -1), (-1, 1, 1), (1, 1, 1), (1, 1, -1)]),
        ((0, -1, 0), [(-1, -1, 1), (-1, -1, -1), (1, -1, -1), (1, -1, 1)]),
        ((0, 0, 1), [(1, -1, 1), (1, 1, 1), (-1, 1, 1), (-1, -1, 1)]),
        ((0, 0, -1), [(-1, -1, -1), (-1, 1, -1), (1, 1, -1), (1, -1, -1)]),
    ]
    ln = math.sqrt(sum(v * v for v in light))
    light = tuple(v / ln for v in light)
    d = ImageDraw.Draw(img, "RGBA")
    drawn = []
    for (n, quad), c in zip(faces, colors):
        nr = rot(n, yaw, pitch)
        if nr[2] >= 0:  # facing away (the eye looks down +z)
            continue
        k = 0.62 + 0.5 * max(0.0, sum(a * b for a, b in zip(nr, light)))
        pts = [project(rot(v, yaw, pitch), cx, cy, s) for v in quad]
        depth = sum(rot(v, yaw, pitch)[2] for v in quad)
        drawn.append((depth, pts, shade(c, k), k))
    for _, pts, c, k in sorted(drawn, key=lambda t: -t[0]):
        d.polygon(pts, fill=c + (255,))
        # Bevel: a lighter inset toward the face's center.
        mx = sum(p[0] for p in pts) / 4
        my = sum(p[1] for p in pts) / 4
        inset = [(mx + (x - mx) * 0.8, my + (y - my) * 0.8) for x, y in pts]
        d.polygon(inset, fill=lighten(c, 0.12) + (255,))
        d.line(pts + [pts[0]], fill=(255, 255, 255, int(edge * min(1.0, k))), width=max(1, int(s / 22)))


def pack(img):
    """RGB image to (w, h, palette RGB565 x 256, indices)."""
    q = img.convert("RGB").quantize(colors=256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.FLOYDSTEINBERG)
    pal = q.getpalette()[: 256 * 3]
    pal += [0] * (256 * 3 - len(pal))
    pal565 = b"".join(
        struct.pack("<H", (pal[i] >> 3) << 11 | (pal[i + 1] >> 2) << 5 | pal[i + 2] >> 3)
        for i in range(0, 256 * 3, 3)
    )
    return img.width, img.height, pal565, q.tobytes()


def write(path, images):
    recs, blobs = [], []
    off = 1 + 8 * len(images)
    for img in images:
        w, h, pal, px = pack(img)
        recs.append(struct.pack("<HHI", w, h, off))
        blobs.append(pal + px)
        off += len(pal) + len(px)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "wb") as f:
        f.write(bytes([len(images)]) + b"".join(recs) + b"".join(blobs))
    return off


def sizes(master):
    """The square master filtered down to the icon."""
    return [master.resize((ICON, ICON), Image.LANCZOS).convert("RGB")]


# Game icons: square masters at ICON x SS.

def cover_cube():
    """Cube: the lit six-color cube of the game, floating over the hills."""
    w = h = ICON * SS
    rng = random.Random(7)
    img = gradient(w, h, [(0, (96, 170, 236)), (0.38, (176, 220, 246)), (0.56, (255, 220, 178)), (0.62, (252, 196, 150)), (1, (252, 196, 150))]).convert("RGBA")
    # Sun behind the cube, rays.
    sun = layer(img)
    sd = ImageDraw.Draw(sun)
    cx, cy = w // 2, int(h * 0.56)
    for i in range(18):
        a = i * math.pi / 9
        sd.polygon([(cx, cy), (cx + math.cos(a - 0.07) * w, cy + math.sin(a - 0.07) * w), (cx + math.cos(a + 0.07) * w, cy + math.sin(a + 0.07) * w)], fill=(255, 250, 225, 34))
    sd.ellipse((cx - 130, cy - 130, cx + 130, cy + 130), fill=(255, 244, 200, 200))
    glow(img, sun, 40)
    # Clouds.
    cl = layer(img)
    cd = ImageDraw.Draw(cl)
    for _ in range(26):
        x, y = rng.uniform(-60, w + 60), rng.uniform(40, h * 0.42)
        rx, ry = rng.uniform(70, 170), rng.uniform(22, 48)
        cd.ellipse((x - rx, y - ry, x + rx, y + ry), fill=(255, 255, 255, 120))
    glow(img, cl, 14, 0.6)
    # Mountain ranges, far (hazy) to near (green).
    for base, rough, col in [(0.56, 120, (128, 156, 200)), (0.62, 90, (104, 136, 178)), (0.70, 70, (86, 150, 120)), (0.80, 50, (58, 128, 84)), (0.90, 30, (40, 104, 64))]:
        pts = ridge(rng, w, h * base, rough)
        ImageDraw.Draw(img).polygon(pts + [(w, h), (0, h)], fill=col + (255,))
    # The cube and its small echoes.
    # The game's face colors, red and blue toward the eye.
    colors = [(60, 150, 230), (180, 90, 220), (240, 200, 60), (90, 200, 100), (240, 240, 240), (230, 70, 60)]
    halo = layer(img)
    ImageDraw.Draw(halo).ellipse((cx - 210, cy - 170, cx + 210, cy + 230), fill=(255, 236, 170, 150))
    glow(img, halo, 60, 0.8)
    shadow = layer(img)
    ImageDraw.Draw(shadow).ellipse((cx - 150, h * 0.9 - 22, cx + 150, h * 0.9 + 22), fill=(10, 40, 20, 150))
    glow(img, shadow, 18, 0.7)
    cube3d(img, cx, cy + 50, 125, 0.62, -0.5, colors)
    for x, y, s, yw, c in [(110, 420, 28, 1.1, (180, 90, 220)), (655, 380, 36, 0.3, (90, 200, 100)), (95, 675, 24, 0.8, (240, 200, 60)), (680, 650, 26, 1.4, (60, 150, 230))]:
        cube3d(img, x, y, s, yw, -0.45, [c] * 6, edge=200)
    logo(img, "CUBE", cx, int(h * 0.15), 220, [(0, (255, 255, 255)), (1, (255, 214, 92))], (28, 42, 96), glow_c=(255, 255, 255))
    ribbon = layer(img)
    rd = ImageDraw.Draw(ribbon)
    rw, ry = 290, int(h * 0.275)
    rd.rounded_rectangle((cx - rw, ry - 30, cx + rw, ry + 30), 30, fill=(28, 42, 96, 255))
    rd.rounded_rectangle((cx - rw + 7, ry - 23, cx + rw - 7, ry + 23), 23, fill=(54, 140, 232, 255))
    rd.text((cx, ry), "LIGHT  ·  COLOR  ·  3D", font=font("black", 34), fill=(255, 255, 255, 255), anchor="mm")
    img.alpha_composite(ribbon)
    return img


def cover_starlane():
    """Starlane: the ship down the neon lane at dusk, blocks and gems."""
    w = h = ICON * SS
    rng = random.Random(11)
    hy = int(h * 0.55)
    img = gradient(w, h, [(0, (8, 10, 34)), (0.3, (46, 30, 92)), (0.5, (150, 66, 128)), (0.5, (20, 12, 44)), (1, (8, 6, 22))]).convert("RGBA")
    cx = w // 2
    # Stars.
    st = layer(img)
    sd = ImageDraw.Draw(st)
    for _ in range(170):
        x, y, r = rng.uniform(0, w), rng.uniform(0, hy - 60), rng.choice([1.5, 2, 2.5, 3.5])
        a = rng.randint(120, 255)
        sd.ellipse((x - r, y - r, x + r, y + r), fill=(235, 240, 255, a))
    glow(img, st, 3)
    # Striped sun on the horizon.
    sun = layer(img)
    sr = 180
    ImageDraw.Draw(sun).ellipse((cx - sr, hy - sr - 30, cx + sr, hy + sr - 30), fill=(255, 255, 255, 255))
    grad = gradient(w, h, [(0, (255, 236, 120)), (0.3, (255, 236, 120)), (0.5, (255, 70, 150)), (1, (255, 70, 150))]).convert("RGBA")
    grad.putalpha(sun.getchannel("A"))
    cut = ImageDraw.Draw(grad)
    for i in range(7):
        y = hy - 125 + i * 22
        cut.rectangle((0, y, w, y + 4 + i * 2), fill=(0, 0, 0, 0))
    halo = layer(img)
    ImageDraw.Draw(halo).ellipse((cx - sr - 50, hy - sr - 80, cx + sr + 50, hy + sr + 10), fill=(255, 90, 170, 140))
    glow(img, halo, 50, 0.9)
    img.alpha_composite(grad)
    floor = gradient(w, h - hy, [(0, (30, 14, 58)), (1, (6, 4, 18))]).convert("RGBA")
    img.alpha_composite(floor, (0, hy))
    # Neon grid toward the vanishing point.
    grid = layer(img)
    gd = ImageDraw.Draw(grid)
    for i in range(-14, 15):
        gd.line((cx, hy, cx + i * 150, h), fill=(60, 220, 255, 150), width=3)
    z = 1.0
    while z < 40:
        y = hy + (h - hy) / z
        gd.line((0, y, w, y), fill=(60, 220, 255, int(170 / z ** 0.3)), width=3)
        z *= 1.32
    # The lane: two magenta edges.
    for s in (-1, 1):
        gd.line((cx + s * 8, hy, cx + s * 330, h), fill=(255, 80, 200, 255), width=7)
    glow(img, grid, 8, 1.3)

    def at(lx, z):
        """Lane point (lx in lane half-widths, z distance) to the screen."""
        k = 1 / z
        return cx + lx * 330 * k, hy + (h - hy) * k, k

    # Blocks.
    for lx, z, c in [(-0.55, 3.2, (255, 110, 70)), (0.6, 5.0, (255, 150, 60)), (0.0, 8.5, (255, 90, 90)), (-0.6, 13, (255, 130, 70))]:
        x, y, k = at(lx, z)
        s = 230 * k
        bl = layer(img)
        bd = ImageDraw.Draw(bl)
        bd.polygon([(x - s, y - s * 0.95), (x - s * 0.8, y - s * 1.25), (x + s * 0.8, y - s * 1.25), (x + s, y - s * 0.95)], fill=lighten(c, 0.35) + (255,))
        bd.rectangle((x - s, y - s * 0.95, x + s, y), fill=c + (255,))
        bd.rectangle((x - s, y - s * 0.95, x + s, y), outline=(255, 230, 200, 255), width=max(2, int(s / 14)))
        glow(img, bl, max(4, s / 5), 0.8)
    # Gems.
    for lx, z in [(0.55, 2.4), (-0.1, 4.2), (0.35, 7.0), (-0.45, 10.5)]:
        x, y, k = at(lx, z)
        s = 120 * k
        y -= s * 2.2
        gm = layer(img)
        gd2 = ImageDraw.Draw(gm)
        gd2.polygon([(x, y - s * 1.4), (x - s, y), (x, y + s * 1.4), (x + s, y)], fill=(70, 230, 255, 255))
        gd2.polygon([(x, y - s * 1.4), (x - s, y), (x, y)], fill=(200, 250, 255, 255))
        gd2.polygon([(x, y), (x + s, y), (x, y + s * 1.4)], fill=(30, 150, 230, 255))
        glow(img, gm, max(4, s / 2), 1.5)
    # The ship, from behind.
    sx, sy = cx, int(h * 0.83)
    eng = layer(img)
    ed = ImageDraw.Draw(eng)
    for dx in (-70, 70):
        ed.ellipse((sx + dx - 30, sy + 10, sx + dx + 30, sy + 70), fill=(80, 220, 255, 255))
    glow(img, eng, 26, 2.0)
    shp = ImageDraw.Draw(img)
    shp.polygon([(sx, sy - 120), (sx - 250, sy + 60), (sx - 90, sy + 40), (sx, sy + 70), (sx + 90, sy + 40), (sx + 250, sy + 60)], fill=(214, 222, 240, 255))
    shp.polygon([(sx, sy - 120), (sx - 250, sy + 60), (sx - 90, sy + 40)], fill=(160, 170, 200, 255))
    shp.polygon([(sx, sy - 120), (sx + 90, sy + 40), (sx, sy + 70)], fill=(240, 244, 255, 255))
    shp.polygon([(sx, sy - 70), (sx - 40, sy + 10), (sx + 40, sy + 10)], fill=(40, 200, 255, 255))
    shp.polygon([(sx, sy - 70), (sx - 40, sy + 10), (sx, sy - 10)], fill=(150, 240, 255, 255))
    for dx in (-250, 250):
        shp.line((sx, sy - 120, sx + dx, sy + 60), fill=(255, 255, 255, 255), width=3)
    logo(img, "STARLANE", cx, int(h * 0.13), 118, [(0, (255, 246, 170)), (0.55, (255, 120, 190)), (1, (120, 220, 255))], (24, 10, 52), kind="blackitalic", glow_c=(255, 90, 200))
    sub = ImageDraw.Draw(img)
    sub.text((cx, int(h * 0.225)), "DODGE  ·  COLLECT  ·  FLY", font=font("black", 34), fill=(150, 236, 255, 255), anchor="mm", stroke_width=5, stroke_fill=(24, 10, 52))
    return img


def cover_plain(title, color):
    """Any other game: its title on its color."""
    w = h = ICON * SS
    dark = shade(color, 0.25)
    img = gradient(w, h, [(0, lighten(color, 0.2)), (0.6, color), (1, dark)]).convert("RGBA")
    rings = layer(img)
    rd = ImageDraw.Draw(rings)
    for r in range(120, 900, 90):
        rd.ellipse((w / 2 - r, h * 0.62 - r, w / 2 + r, h * 0.62 + r), outline=(255, 255, 255, 40), width=6)
    img.alpha_composite(rings)
    words = title.split()
    lines, cur = [], ""
    for wd in words:
        if cur and len(cur) + len(wd) > 7:
            lines.append(cur)
            cur = wd
        else:
            cur = (cur + " " + wd).strip()
    lines.append(cur)
    for i, ln in enumerate(lines[:3]):
        logo(img, ln.upper(), w // 2, int(h * 0.22) + i * 170, 140, [(0, (255, 255, 255)), (1, lighten(color, 0.6))], dark)
    return img


def cover_lumen():
    """Lumen: a valley at dusk, pine silhouettes, a light to collect."""
    w = h = ICON * SS
    rng = random.Random(11)
    img = gradient(w, h, [(0, (22, 26, 70)), (0.35, (88, 62, 140)), (0.55, (238, 128, 96)), (0.64, (255, 196, 120)), (1, (255, 196, 120))]).convert("RGBA")
    cx, cy = w // 2, int(h * 0.50)
    # The sun low on the horizon and its rays.
    sun = layer(img)
    sd = ImageDraw.Draw(sun)
    for i in range(14):
        a = i * math.pi / 7
        sd.polygon([(cx, cy), (cx + math.cos(a - 0.05) * w, cy + math.sin(a - 0.05) * w), (cx + math.cos(a + 0.05) * w, cy + math.sin(a + 0.05) * w)], fill=(255, 226, 170, 26))
    sd.ellipse((cx - 100, cy - 100, cx + 100, cy + 100), fill=(255, 238, 190, 230))
    glow(img, sun, 36)
    for base, rough, col in [(0.58, 110, (112, 82, 150)), (0.66, 80, (82, 70, 122)), (0.76, 60, (50, 62, 96)), (0.88, 36, (28, 40, 70))]:
        pts = ridge(rng, w, h * base, rough)
        ImageDraw.Draw(img).polygon(pts + [(w, h), (0, h)], fill=col + (255,))
    # Pines along the foreground.
    d = ImageDraw.Draw(img)
    for _ in range(26):
        x = rng.uniform(-20, w + 20)
        base = h * rng.uniform(0.86, 1.0)
        ph = rng.uniform(120, 260)
        for k in range(3):
            tw = ph * (0.30 - 0.07 * k)
            top = base - ph * (0.45 + 0.28 * k)
            d.polygon([(x, top - ph * 0.30), (x - tw, top + ph * 0.18), (x + tw, top + ph * 0.18)], fill=(14, 28, 40, 255))
    # The light: a golden orb with a halo.
    lx, ly = int(w * 0.5), int(h * 0.70)
    halo = layer(img)
    ImageDraw.Draw(halo).ellipse((lx - 150, ly - 150, lx + 150, ly + 150), fill=(255, 220, 110, 170))
    glow(img, halo, 54, 0.9)
    orb = layer(img)
    od = ImageDraw.Draw(orb)
    od.polygon([(lx, ly - 70), (lx + 52, ly), (lx, ly + 70), (lx - 52, ly)], fill=(255, 236, 150, 255))
    od.polygon([(lx, ly - 70), (lx + 52, ly), (lx, ly)], fill=(255, 250, 215, 255))
    img.alpha_composite(orb)
    logo(img, "LUMEN", cx, int(h * 0.16), 200, [(0, (255, 255, 255)), (1, (255, 206, 110))], (40, 24, 70), glow_c=(255, 220, 150))
    return img


GAMES = {"cube": cover_cube, "starlane": cover_starlane, "lumen": cover_lumen}


# The UI's own art: glyphs in `col`, about 0.55 x `s` across.

def ic_system(d, cx, cy, s, col):
    a, t = s * 0.21, int(s * 0.05)
    d.rounded_rectangle((cx - a, cy - a, cx + a, cy + a), s * 0.045, outline=col, width=t)
    b = a * 0.46
    d.rounded_rectangle((cx - b, cy - b, cx + b, cy + b), s * 0.02, outline=col, width=max(1, int(s * 0.04)))
    for i in (-0.58, 0, 0.58):
        for (x0, y0, x1, y1) in [
            (cx + i * a, cy - a * 1.42, cx + i * a, cy - a),
            (cx + i * a, cy + a, cx + i * a, cy + a * 1.42),
            (cx - a * 1.42, cy + i * a, cx - a, cy + i * a),
            (cx + a, cy + i * a, cx + a * 1.42, cy + i * a),
        ]:
            d.line((x0, y0, x1, y1), fill=col, width=t)


def ic_network(d, cx, cy, s, col):
    r, t = s * 0.27, int(s * 0.045)
    d.ellipse((cx - r, cy - r, cx + r, cy + r), outline=col, width=t)
    d.ellipse((cx - r * 0.45, cy - r, cx + r * 0.45, cy + r), outline=col, width=t)
    d.line((cx, cy - r, cx, cy + r), fill=col, width=t)
    for k in (-0.5, 0, 0.5):
        hw = r * math.sqrt(1 - k * k)
        d.line((cx - hw, cy + k * r, cx + hw, cy + k * r), fill=col, width=t)


def ic_terminal(d, cx, cy, s, col):
    a, b = s * 0.3, s * 0.23
    t = int(s * 0.045)
    d.rounded_rectangle((cx - a, cy - b, cx + a, cy + b), s * 0.05, outline=col, width=t)
    t = int(s * 0.055)
    d.line((cx - a * 0.55, cy - b * 0.35, cx - a * 0.15, cy + b * 0.05), fill=col, width=t, joint="curve")
    d.line((cx - a * 0.15, cy + b * 0.05, cx - a * 0.55, cy + b * 0.45), fill=col, width=t, joint="curve")
    d.line((cx + a * 0.05, cy + b * 0.45, cx + a * 0.55, cy + b * 0.45), fill=col, width=t)


def ic_logs(d, cx, cy, s, col):
    a, b = s * 0.21, s * 0.28
    t = int(s * 0.045)
    d.rounded_rectangle((cx - a, cy - b, cx + a, cy + b), s * 0.04, outline=col, width=t)
    for i, k in enumerate((0.55, 0.8, 0.8, 0.45)):
        y = cy - b * 0.5 + i * b * 0.34
        d.line((cx - a * 0.55, y, cx - a * 0.55 + a * 1.1 * k, y), fill=col, width=t)


def ic_settings(d, cx, cy, s, col):
    """Switch settings sun-gear: a hollow ring with eight radial spokes."""
    ri, ro, t = s * 0.175, s * 0.275, int(s * 0.052)
    for i in range(8):
        a = i * math.pi / 4
        ca, sa = math.cos(a), math.sin(a)
        d.line((cx + ca * (ri - t * 0.3), cy + sa * (ri - t * 0.3), cx + ca * ro, cy + sa * ro), fill=col, width=t)
    d.ellipse((cx - ri, cy - ri, cx + ri, cy + ri), outline=col, width=int(s * 0.055))


def ic_power(d, cx, cy, s, col):
    r, t = s * 0.23, int(s * 0.055)
    d.arc((cx - r, cy - r, cx + r, cy + r), -55, 235, fill=col, width=t)
    d.line((cx, cy - r * 1.2, cx, cy - r * 0.15), fill=col, width=t)


def ic_controllers(d, cx, cy, s, col):
    """A pair of Joy-Cons side by side, each with its stick."""
    w, h, t, g = s * 0.17, s * 0.44, int(s * 0.045), s * 0.035
    for x0 in (cx - g - w, cx + g):
        d.rounded_rectangle((x0, cy - h / 2, x0 + w, cy + h / 2), w * 0.45, outline=col, width=t)
    r = w * 0.27
    for x, y in ((cx - g - w / 2, cy - h * 0.2), (cx + g + w / 2, cy + h * 0.1)):
        d.ellipse((x - r, y - r, x + r, y + r), fill=col)


# Home order: System, Network, Terminal, Logs, Controllers, System Settings, Power.
GLYPHS = [ic_system, ic_network, ic_terminal, ic_logs, ic_controllers, ic_settings, ic_power]
# Glyph colors; None: the theme's grey.
COLORS = [(240, 86, 62), (70, 136, 241), (58, 190, 192), (44, 182, 67), (247, 174, 36), None, None]
THEMES = {
    # (page background, dock surface, grey glyph)
    "dark": ((28, 28, 28), (46, 46, 46), (196, 196, 196)),
    "dark_sel": ((28, 28, 28), (60, 60, 60), (220, 220, 220)),
    "light": ((235, 235, 235), (255, 255, 255), (88, 88, 88)),
}


def glyph_img(size, bg, k, grey, scale):
    s = size * SS
    img = Image.new("RGBA", (s, s), bg + (255,))
    ic = layer(img)
    GLYPHS[k](ImageDraw.Draw(ic), s / 2, s / 2, s * scale, (COLORS[k] or grey) + (255,))
    img.alpha_composite(ic)
    return img.resize((size, size), Image.LANCZOS).convert("RGB")


def buttons(theme):
    """The dock buttons on the dock pill or selected circle background."""
    _, disc, grey = THEMES[theme]
    return [glyph_img(BUTTON, disc, k, grey, 0.8) for k in range(len(GLYPHS))]


def heads(theme):
    bg, _, grey = THEMES[theme]
    return [glyph_img(HEAD, bg, k, grey, 1.45) for k in range(len(GLYPHS))]


def avatar(size):
    s = AVATAR * SS * 2
    img = gradient(s, s, [(0, (255, 168, 76)), (1, (226, 84, 28))]).convert("RGBA")
    g = mklogo.icon()
    n = len(g)
    cell = s * 0.62 / n
    x0 = y0 = (s - cell * n) / 2
    pal = {1: (59, 26, 12), 2: (90, 30, 10), 3: (130, 44, 14), 4: (255, 255, 255), 5: (255, 238, 214), 6: (255, 250, 240), 7: (255, 255, 255)}
    d = ImageDraw.Draw(img)
    for y in range(n):
        for x in range(n):
            if g[y][x]:
                d.rectangle((x0 + x * cell, y0 + y * cell, x0 + (x + 1) * cell, y0 + (y + 1) * cell), fill=pal[g[y][x]] + (255,))
    return img.convert("RGB").resize((size, size), Image.LANCZOS)


def game_info(path):
    title, color = None, (36, 168, 248)
    if os.path.exists(path):
        for line in open(path, encoding="utf-8"):
            k, _, v = line.partition("=")
            v = v.strip()
            if k.strip() == "title":
                title = v
            elif k.strip() == "color":
                try:
                    c = int(v.lstrip("#"), 16)
                    color = (c >> 16 & 255, c >> 8 & 255, c & 255)
                except ValueError:
                    pass
    return title, color


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", required=True, help="the UI's art (avatar, buttons, header glyphs)")
    ap.add_argument("--games", help="directory of the games' covers")
    a = ap.parse_args()
    # In art.rs' order: AVATAR, AVATAR_SMALL, BUTTON (dark, dark_sel, light), HEAD (dark, light).
    ui = (
        [avatar(AVATAR), avatar(44)]
        + buttons("dark")
        + buttons("dark_sel")
        + buttons("light")
        + heads("dark")
        + heads("light")
    )
    n = write(a.o, ui)
    print(f"mkcovers: {a.o}: {n} bytes")
    if a.games:
        src = os.path.join(ROOT, "jeux")
        for name in sorted(os.listdir(src)):
            info = os.path.join(src, name, "game.txt")
            if not os.path.exists(info):
                continue
            title, color = game_info(info)
            master = GAMES[name]() if name in GAMES else cover_plain(title or name, color)
            out = os.path.join(a.games, name + ".bin")
            n = write(out, sizes(master))
            print(f"mkcovers: {out}: {n} bytes")


if __name__ == "__main__":
    main()
