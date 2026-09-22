#!/usr/bin/env python3
"""Broadcom Wi-Fi firmware and calibration data out of the Switch's `wlan`
system title (and the Bluetooth firmware out of `bluetooth`).

Takes the ExeFS `main` of the title, which is an NSO0 image, or an ELF
already converted by nx2elf. Markers are the ones nx-fwextract uses
(index.js, GPL-3.0, perillamint); this rewrite drops the Node.js and
nx2elf dependencies: an NSO is read directly.

    nxfw-extract.py wlan main -o ~/.cache/nxk/firmware
    nxfw-extract.py bluetooth main -o ~/.cache/nxk/firmware
"""

import argparse
import ctypes
import ctypes.util
import os
import struct
import sys

# --------------------------------------------------------------- LZ4 block

def _lz4_c():
    """LZ4_decompress_safe from the system library, if there is one."""
    for name in ("lz4", None):
        path = ctypes.util.find_library(name) if name else "liblz4.so.1"
        if not path:
            continue
        try:
            lib = ctypes.CDLL(path)
            fn = lib.LZ4_decompress_safe
        except (OSError, AttributeError):
            continue
        fn.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        fn.restype = ctypes.c_int
        return fn
    return None


def _lz4_python(src, out_size):
    """LZ4 block format, byte by byte: the fallback when no library is
    around. Slow but exact."""
    dst = bytearray()
    i = 0
    n = len(src)
    while i < n:
        token = src[i]
        i += 1
        length = token >> 4
        if length == 15:
            while True:
                b = src[i]
                i += 1
                length += b
                if b != 255:
                    break
        dst += src[i:i + length]
        i += length
        if i >= n:
            break
        offset = src[i] | (src[i + 1] << 8)
        i += 2
        match = token & 0xF
        if match == 15:
            while True:
                b = src[i]
                i += 1
                match += b
                if b != 255:
                    break
        match += 4
        start = len(dst) - offset
        if start < 0:
            raise ValueError("LZ4: offset before the start of the output")
        # Overlapping copies are legal and common: copy in pieces.
        while match > 0:
            step = min(match, len(dst) - start)
            dst += dst[start:start + step]
            start += step
            match -= step
    if len(dst) != out_size:
        raise ValueError(f"LZ4: {len(dst)} bytes instead of {out_size}")
    return bytes(dst)


def lz4_decompress(src, out_size):
    try:
        import lz4.block  # noqa: F401  (present on some systems)
        return lz4.block.decompress(src, uncompressed_size=out_size)
    except ImportError:
        pass
    fn = _lz4_c()
    if fn is not None:
        out = ctypes.create_string_buffer(out_size)
        got = fn(src, out, len(src), out_size)
        if got != out_size:
            raise ValueError(f"LZ4: library returned {got}, expected {out_size}")
        return out.raw
    return _lz4_python(src, out_size)


# ------------------------------------------------------------------ NSO/ELF

def nso_segments(blob):
    """`.text`, `.rodata` and `.data` of an NSO0 image (switchbrew NSO)."""
    if blob[:4] != b"NSO0":
        raise ValueError("not an NSO0 image")
    flags = struct.unpack_from("<I", blob, 0x0C)[0]
    out = {}
    # Segment headers at 0x10, 0x20 and 0x30; compressed sizes at 0x60.
    for i, name in enumerate((".text", ".rodata", ".data")):
        file_off, _mem_off, size = struct.unpack_from("<III", blob, 0x10 + i * 0x10)
        comp_size = struct.unpack_from("<I", blob, 0x60 + i * 4)[0]
        raw = blob[file_off:file_off + comp_size]
        out[name] = lz4_decompress(raw, size) if flags & (1 << i) else raw[:size]
    return out


def elf_sections(blob):
    """`.data` and `.rodata` of a 64-bit little-endian ELF."""
    if blob[:4] != b"\x7fELF":
        raise ValueError("not an ELF file")
    if blob[4] != 2 or blob[5] != 1:
        raise ValueError("only 64-bit little-endian ELF is supported")
    shoff, = struct.unpack_from("<Q", blob, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", blob, 0x3A)
    def entry(i):
        return struct.unpack_from("<IIQQQQIIQQ", blob, shoff + i * shentsize)
    str_off = entry(shstrndx)[4]
    out = {}
    for i in range(shnum):
        name_off, _type, _flags, _addr, off, size = entry(i)[:6]
        end = blob.index(b"\0", str_off + name_off)
        out[blob[str_off + name_off:end].decode()] = blob[off:off + size]
    return out


def segments(path):
    blob = open(path, "rb").read()
    if blob[:4] == b"NSO0":
        return nso_segments(blob)
    if blob[:4] == b"\x7fELF":
        return elf_sections(blob)
    raise ValueError("expected an NSO0 (ExeFS `main`) or an ELF")


# ------------------------------------------------------------- extraction

def cut(data, start, what):
    """From `start` to the next NUL byte."""
    end = data.find(b"\0", start)
    if end < 0:
        raise ValueError(f"{what}: no end marker")
    return data[start:end]


def extract_wlan(data, outdir, name):
    """Calibration data then the firmware image, as nx-fwextract does:
    the NVRAM starts at its `NVRAMRev=` line, the firmware at the byte
    after the NUL preceding Broadcom's `DBPP` debug tag and ends at the
    NUL after its version string."""
    off = data.find(b"NVRAMRev=")
    if off < 0:
        raise ValueError("calibration data not found (no NVRAMRev=)")
    caldata = cut(data, off, "calibration data")
    print(f"  calibration data: {off:#x}, {len(caldata)} bytes")

    dbpp = data.find(b"DBPP")
    if dbpp < 0:
        raise ValueError("firmware not found (no DBPP tag)")
    start = -1
    for i in range(dbpp, max(dbpp - 1024, 0), -1):
        if data[i] == 0:
            start = i + 1
            break
    if start < 0:
        raise ValueError("firmware: no NUL before the DBPP tag")
    tail = data.find(b"4356a3-roml", start)
    if tail < 0:
        raise ValueError("firmware: version string 4356a3-roml not found "
                         "(different chip revision?)")
    end = data.find(b"\0", tail)
    firmware = data[start:end]
    print(f"  firmware:         {start:#x}, {len(firmware)} bytes")
    print(f"  version:          {data[tail:end].decode(errors='replace')[:60]}")

    write(os.path.join(outdir, f"{name}.txt"), caldata)
    write(os.path.join(outdir, f"{name}.bin"), firmware)


def extract_bt(data, outdir, name):
    start = data.find(bytes.fromhex("4CFC4600"))
    end = data.find(bytes.fromhex("FC04FFFFFFFF"))
    if start < 0 or end < 0:
        raise ValueError("Bluetooth firmware markers not found")
    end += 6
    print(f"  firmware:         {start:#x}, {end - start} bytes")
    write(os.path.join(outdir, name), data[start:end])


def write(path, blob):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "wb") as f:
        f.write(blob)
    print(f"  -> {path} ({len(blob)} bytes)")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=("wlan", "bluetooth"))
    ap.add_argument("image", help="ExeFS `main` (NSO0) or ELF of the title")
    ap.add_argument("-o", "--outdir", default=".", help="output directory")
    ap.add_argument("--name", help="base name of the output files")
    a = ap.parse_args()

    try:
        secs = segments(a.image)
    except ValueError as e:
        sys.exit(f"{a.image}: {e}")

    if a.kind == "wlan":
        data = secs.get(".data")
        if not data:
            sys.exit("no .data segment in the image")
        print(f"{a.image}: .data {len(data)} bytes")
        extract_wlan(data, a.outdir, a.name or "brcmfmac4356-pcie")
    else:
        data = secs.get(".rodata")
        if not data:
            sys.exit("no .rodata segment in the image")
        print(f"{a.image}: .rodata {len(data)} bytes")
        extract_bt(data, a.outdir, a.name or "BCM4356A3.hcd")


if __name__ == "__main__":
    main()
