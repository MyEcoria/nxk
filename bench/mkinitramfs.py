#!/usr/bin/env python3
"""Construit un initramfs minimal (cpio « newc ») : /init, /dev/console,
/proc, /sys. Aucune distribution : seul le noyau L4T est mesure."""
import argparse, os


def entry(name, mode, data=b"", rdev=(0, 0), ino=[1]):
    ino[0] += 1
    name_b = name.encode() + b"\0"
    hdr = "070701" + "".join(f"{v:08x}" for v in [
        ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name_b), 0])
    out = hdr.encode() + name_b
    out += b"\0" * ((-len(out)) % 4)
    out += data
    out += b"\0" * ((-len(out)) % 4)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--init", required=True)
    ap.add_argument("-o", "--output", required=True)
    a = ap.parse_args()
    img = b""
    img += entry("dev", 0o040755)
    img += entry("dev/console", 0o020600, rdev=(5, 1))
    img += entry("proc", 0o040755)
    img += entry("sys", 0o040755)
    img += entry("init", 0o100755, open(a.init, "rb").read())
    img += entry("TRAILER!!!", 0)
    img += b"\0" * ((-len(img)) % 512)
    open(a.output, "wb").write(img)
    print(f"initramfs : {len(img)} octets -> {a.output}")


main()
