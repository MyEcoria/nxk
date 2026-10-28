#!/usr/bin/env python3
"""DRAM parameter tables for nxk-ipl, taken from hekate's sources.

    sdramgen.py [--hekate refs/hekate] -o hwboot/nxk-ipl/src/sdram_tables.rs

For each SoC: the parameter structure (sdram_param_t210*.h, all u32
fields, in order), hekate's base table (sdram_config*.inl designated
initializers, missing fields zero) and its vendor patches (value, DRAM
id or code mask, field index). The T210B01 DRAM id -> code table comes
from sdram.c. Constants in the values are resolved from hdr.rs's set.
The output is committed: the build does not need refs/.
"""
import argparse
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

FIELD = re.compile(r"^\s*u32\s+(\w+)\s*;")
INIT = re.compile(r"^\s*\.(\w+)\s*=\s*([^,]+?)\s*,?\s*(?://.*)?$")
PATCH = re.compile(r"^\s*\{\s*([^,]+),\s*([^,]+),\s*DCFG_OFFSET_OF\((\w+)\)\s*\}")
ENUM = re.compile(r"^\s*(\w+)\s*=\s*(\d+)\s*,")


def strip_comments(text):
    return re.sub(r"/\*.*?\*/", " ", text, flags=re.S)


def fields(path, name):
    text = strip_comments(open(path).read())
    body = text[text.index(f"typedef struct _{name}"):text.index(f"}} {name};")]
    return [m.group(1) for line in body.splitlines() if (m := FIELD.match(line))]


MACROS = {}


def load_macros(paths):
    """Function-like `#define NAME(arg) body` macros, one argument."""
    for p in paths:
        for m in re.finditer(r"^\s*#define\s+(\w+)\((\w+)\)\s+(.+?)\s*(?://.*)?$", open(p).read(), re.M):
            MACROS[m.group(1)] = (m.group(2), m.group(3))


def expand(e):
    for _ in range(10):
        done = True
        for name, (arg, body) in MACROS.items():
            m = re.search(rf"\b{name}\(([^()]*)\)", e)
            if m:
                e = e[:m.start()] + "(" + re.sub(rf"\b{arg}\b", f"({m.group(1)})", body) + ")" + e[m.end():]
                done = False
        if done:
            break
    return e


def evaluate(expr, consts):
    e = re.sub(r"\bDRAM_(ID|CC)\(", "BIT(", " ".join(expr.split()))
    e = expand(e)
    e = re.sub(r"(\b0[xX][0-9a-fA-F]+|\b\d+)[uUlL]+\b", r"\1", e)
    e = re.sub(r"\bBIT\(", "_BIT(", e)
    for w in set(re.findall(r"\b[A-Za-z_]\w*\b", e)) - {"_BIT"}:
        if w not in consts:
            raise SystemExit(f"unknown constant {w} in {expr!r}")
        e = re.sub(rf"\b{w}\b", str(consts[w]), e)
    return eval(e, {"__builtins__": {}}, {"_BIT": lambda n: 1 << n}) & 0xFFFF_FFFF


def table(path, var, names, consts):
    text = re.sub(r"//[^\n]*", "", strip_comments(open(path).read()))
    start = text.index(var)
    body = text[text.index("{", start) + 1:text.index("};", start)]
    vals = [0] * len(names)
    index = {n: i for i, n in enumerate(names)}
    for m in re.finditer(r"\.(\w+)\s*=\s*([^.]+?)\s*(?:,\s*(?=\.|$)|$)", body.strip()):
        vals[index[m.group(1)]] = evaluate(m.group(2).rstrip(",").strip(), consts)
    return vals


def patches(path, var, names, consts):
    text = strip_comments(open(path).read())
    start = text.index(var)
    body = text[start:text.index("};", start)]
    index = {n: i for i, n in enumerate(names)}
    out = []
    for line in body.splitlines():
        if m := PATCH.match(line):
            out.append((evaluate(m.group(1), consts), evaluate(m.group(2), consts), index[m.group(3)]))
    return out


def enums(path):
    return {m.group(1): int(m.group(2)) for line in open(path) if (m := ENUM.match(line))}


def emit(out, name, names, base, pats):
    out.append("#[repr(C)]")
    out.append(f"pub struct {name} {{")
    out += [f"    pub {n}: u32," for n in names]
    out.append("}")
    up = name.upper()
    out.append(f"pub const {up}_WORDS: usize = {len(names)};")
    out.append(f"pub static {up}_BASE: [u32; {len(names)}] = [")
    out += [f"    {v:#010x}, // {n}" for n, v in zip(names, base)]
    out.append("];")
    out.append("/// (value, DRAM id or code mask, word index).")
    out.append(f"pub static {up}_PATCHES: [(u32, u32, u16); {len(pats)}] = [")
    out += [f"    ({v:#010x}, {m:#010x}, {i}), // {names[i]}" for v, m, i in pats]
    out.append("];")


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--hekate", default=os.path.join(here, "..", "..", "refs", "hekate"))
    ap.add_argument("-o", required=True)
    a = ap.parse_args()
    mem = os.path.join(a.hekate, "bdk", "mem")
    consts = {m.group(1): int(m.group(2), 16) for line in open(os.path.join(here, "src", "hdr.rs"))
              if (m := re.match(r"pub const (\w+): u32 = (0x[0-9a-f]+);", line))}
    consts.update(enums(os.path.join(mem, "sdram.h")))
    load_macros([os.path.join(mem, "mc_t210.h"), os.path.join(mem, "emc_t210.h")])
    # Masks the .inl files define for their patches.
    for inl in ("sdram_config.inl", "sdram_config_t210b01.inl"):
        text = strip_comments(open(os.path.join(mem, inl)).read()).replace("\\\n", " ")
        for m in re.finditer(r"^\s*#define\s+(\w+)\s+(.+?)\s*(?://.*)?$", text, re.M):
            try:
                consts[m.group(1)] = evaluate(m.group(2), consts)
            except SystemExit:
                pass

    out = ["// Generated by hwboot/nxk-ipl/sdramgen.py from hekate's DRAM tables (GPL-2.0).",
           "// Do not edit: rerun the script.", "#![allow(dead_code)]", ""]
    t210 = fields(os.path.join(mem, "sdram_param_t210.h"), "sdram_params_t210_t")
    emit(out, "ParamsT210", t210,
         table(os.path.join(mem, "sdram_config.inl"), "_dram_cfg_0_samsung_4gb", t210, consts),
         patches(os.path.join(mem, "sdram_config.inl"), "sdram_cfg_vendor_patches_t210[]", t210, consts))
    out.append("")
    b01 = fields(os.path.join(mem, "sdram_param_t210b01.h"), "sdram_params_t210b01_t")
    emit(out, "ParamsT210b01", b01,
         table(os.path.join(mem, "sdram_config_t210b01.inl"), "_dram_cfg_08_10_12_14_samsung_hynix_4gb", b01, consts),
         patches(os.path.join(mem, "sdram_config_t210b01.inl"), "sdram_cfg_vendor_patches_t210b01[]", b01, consts))

    src = strip_comments(open(os.path.join(mem, "sdram.c")).read())
    enc = src[src.index("dram_encoding_t210b01[] = {"):]
    enc = enc[enc.index("{") + 1:enc.index("};")]
    codes = [evaluate(x, consts) for x in enc.split(",") if x.strip()]
    out.append("")
    out.append("/// T210B01 fused DRAM id -> the code its patches select (0: base table as is).")
    out.append(f"pub static DRAM_ENCODING_T210B01: [u8; {len(codes)}] = {codes};")
    open(a.o, "w").write("\n".join(out) + "\n")
    print(f"T210: {len(t210)} words, B01: {len(b01)} words, {len(codes)} B01 ids -> {a.o}")


main()
