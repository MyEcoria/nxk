#!/usr/bin/env python3
"""Assemble l'image de demarrage de nxk a partir d'executables.

Format (nxk_abi::bootfs) : en-tete "NXKBOOT1" + nombre d'entrees, table
d'entrees (nom sur 24 octets, position, taille), puis les fichiers alignes
sur 16 octets. L'image entiere est completee a un multiple de 4 KiB.
"""
import argparse, os, struct

NAME_LEN = 24


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", "--output", required=True)
    ap.add_argument("files", nargs="+")
    a = ap.parse_args()

    entries, blobs = [], []
    table_end = 16 + 32 * len(a.files)
    off = (table_end + 15) & ~15
    for path in a.files:
        name = os.path.basename(path).encode()
        if len(name) >= NAME_LEN:
            raise SystemExit(f"nom trop long : {name}")
        data = open(path, "rb").read()
        entries.append((name, off, len(data)))
        blobs.append((off, data))
        off = (off + len(data) + 15) & ~15

    img = bytearray(b"NXKBOOT1" + struct.pack("<II", len(entries), 0))
    for name, o, size in entries:
        img += name.ljust(NAME_LEN, b"\0") + struct.pack("<II", o, size)
    for o, data in blobs:
        img += b"\0" * (o - len(img))
        img += data
    img += b"\0" * ((-len(img)) % 4096)

    os.makedirs(os.path.dirname(os.path.abspath(a.output)), exist_ok=True)
    open(a.output, "wb").write(img)
    print(f"bootfs : {len(entries)} programmes, {len(img) // 1024} KiB -> {a.output}")
    for name, o, size in entries:
        print(f"  {name.decode():<12} {size:>7} octets")


main()
