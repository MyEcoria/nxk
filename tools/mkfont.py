#!/usr/bin/env python3
"""8x16 font of the display driver: the first 256 Unicode code points
(ASCII and Latin-1), extracted from a system PSF console font.

Defaults to Terminus (Lat15-Terminus16, SIL Open Font License 1.1, from
console-setup). Without any font, solid blocks. The font is not vendored:
it is converted at build time into build/font.bin (256 x 16 bytes).

    mkfont.py -o build/font.bin [--psf file.psf.gz]
"""
import argparse, gzip, os, struct

DEFAULT = "/usr/share/consolefonts/Lat15-Terminus16.psf.gz"

def load_psf(path):
    raw = gzip.open(path).read() if path.endswith(".gz") else open(path, "rb").read()
    if raw[:2] == b"\x36\x04":  # PSF1
        mode, h = raw[2], raw[3]
        n = 512 if mode & 1 else 256
        w, size, off = 8, h, 4
        glyphs = [raw[off + i * size: off + (i + 1) * size] for i in range(n)]
        table, uni = raw[off + n * size:], {}
        if mode & 2:
            g, p = 0, 0
            while g < n and p + 1 < len(table):
                cp = struct.unpack("<H", table[p:p + 2])[0]
                p += 2
                if cp == 0xFFFF:
                    g += 1
                elif cp != 0xFFFE:
                    uni.setdefault(cp, g)
        return w, h, glyphs, uni
    magic, ver, hdr, flags, n, size, h, w = struct.unpack("<8I", raw[:32])
    assert magic == 0x864AB572, "unknown PSF format"
    glyphs = [raw[hdr + i * size: hdr + (i + 1) * size] for i in range(n)]
    uni, p, g = {}, hdr + n * size, 0
    if flags & 1:
        while g < n and p < len(raw):
            b = raw[p]
            if b == 0xFF:
                g += 1
                p += 1
                continue
            if b == 0xFE:
                p += 1
                continue
            ln = 1 if b < 0x80 else 2 if b < 0xE0 else 3 if b < 0xF0 else 4
            uni.setdefault(ord(raw[p:p + ln].decode("utf-8", "replace")), g)
            p += ln
    return w, h, glyphs, uni

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
ap.add_argument("--psf", default=DEFAULT)
a = ap.parse_args()
out = bytearray()
if os.path.exists(a.psf):
    w, h, glyphs, uni = load_psf(a.psf)
    assert (w, h) == (8, 16), f"font {w}x{h}: expected 8x16"
    for cp in range(256):
        g = uni.get(cp, uni.get(ord("?"), 0)) if uni else cp
        out += glyphs[g][:16]
else:
    for cp in range(256):
        out += bytes(16) if cp <= 0x20 else b"\x00\x00" + b"\x7e" * 12 + b"\x00\x00"
os.makedirs(os.path.dirname(os.path.abspath(a.o)), exist_ok=True)
open(a.o, "wb").write(out)
