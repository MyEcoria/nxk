#!/usr/bin/env python3
"""Minimal DTB a secure monitor passes to nxk (PSCI path tests): 4 GiB of
DRAM and the l4tboot monitor area (2 MiB at 0xFFE00000) reserved.

    mkdtb-moniteur.py -o build/moniteur.dtb
"""
import argparse, struct

def dtb(nodes_struct, strings):
    hdr_size = 40
    rsvmap = struct.pack(">QQ", 0, 0)
    off_rsv = hdr_size
    off_struct = off_rsv + len(rsvmap)
    off_strings = off_struct + len(nodes_struct)
    total = off_strings + len(strings)
    hdr = struct.pack(">10I", 0xD00DFEED, total, off_struct, off_strings, off_rsv, 17, 16, 0,
                      len(strings), len(nodes_struct))
    return hdr + rsvmap + nodes_struct + strings

class Builder:
    def __init__(self):
        self.s = b""
        self.strings = b""
        self.offs = {}
    def _str(self, name):
        if name not in self.offs:
            self.offs[name] = len(self.strings)
            self.strings += name.encode() + b"\0"
        return self.offs[name]
    def _pad(self):
        self.s += b"\0" * (-len(self.s) % 4)
    def begin(self, name):
        self.s += struct.pack(">I", 1) + name.encode() + b"\0"
        self._pad()
    def end(self):
        self.s += struct.pack(">I", 2)
    def prop(self, name, value):
        self.s += struct.pack(">III", 3, len(value), self._str(name)) + value
        self._pad()
    def u32(self, name, *vals):
        self.prop(name, b"".join(struct.pack(">I", v) for v in vals))
    def finish(self):
        self.s += struct.pack(">I", 9)
        return dtb(self.s, self.strings)

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
a = ap.parse_args()
b = Builder()
b.begin("")
b.u32("#address-cells", 2)
b.u32("#size-cells", 2)
b.begin("memory@80000000")
b.prop("device_type", b"memory\0")
b.u32("reg", 0, 0x80000000, 1, 0)
b.end()
b.begin("reserved-memory")
b.u32("#address-cells", 2)
b.u32("#size-cells", 2)
b.prop("ranges", b"")
b.begin("moniteur@ffe00000")
b.u32("reg", 0, 0xFFE00000, 0, 0x200000)
b.prop("no-map", b"")
b.end()
b.end()
b.end()
open(a.o, "wb").write(b.finish())
