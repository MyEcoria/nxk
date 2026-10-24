#!/usr/bin/env python3
"""Writes src/tables.rs from the data tables of Cisco's OpenH264 (BSD-2-Clause),
which carry the numbers of ITU-T H.264 Tables 9-12 to 9-45 and 8-16/8-17.

    tools/gen_tables.py /path/to/openh264/codec > src/tables.rs
"""
import re, sys

root = sys.argv[1]

def read(p):
    return open(f"{root}/{p}").read()

def array(src, name):
    m = re.search(r"\b" + re.escape(name) + r"\s*(\[[^=]*\])\s*=\s*\{", src)
    if not m:
        raise SystemExit("missing " + name)
    i, depth, j = m.end(), 1, m.end()
    while depth:
        c = src[j]
        depth += (c == "{") - (c == "}")
        j += 1
    body = src[i : j - 1]
    body = re.sub(r"/\*.*?\*/", "", body, flags=re.S)
    body = re.sub(r"//[^\n]*", "", body)
    body = re.sub(r"-\s+(?=\d)", "-", body)
    body = body.replace("CTX_NA", "0")  # wels_common_defs.h
    return [int(x) for x in re.findall(r"-?\d+", body)]

common = read("common/src/common_tables.cpp")
basis = read("decoder/core/inc/wels_common_basis.h")
deblock = read("decoder/core/src/deblocking.cpp")

def emit(name, ty, vals, per_line=16, doc=""):
    print(f"/// {doc}")
    print(f"pub static {name}: [{ty}; {len(vals)}] = [")
    for k in range(0, len(vals), per_line):
        print("    " + ", ".join(str(v) for v in vals[k : k + per_line]) + ",")
    print("];\n")

print("""//! Data tables of ITU-T H.264, copied from Cisco's OpenH264 (BSD-2-Clause,
//! copyright (c) 2013 Cisco Systems): `tools/gen_tables.py` writes this file.

/// Contexts kept per slice: ctxIdx 0..460 of the standard.
pub const CTX_COUNT: usize = 460;
""")
init = array(common, "g_kiCabacGlobalContextIdx")
assert len(init) == 460 * 4 * 2, len(init)
print("/// (m, n) of Tables 9-12 to 9-24 for each ctxIdx: model 0 is I slices, 1 to 3 is `cabac_init_idc` 0 to 2.")
print(f"pub static CABAC_INIT: [[[i8; 2]; 4]; 460] = [")
for c in range(460):
    cells = ", ".join(f"[{init[(c*4+k)*2]}, {init[(c*4+k)*2+1]}]" for k in range(4))
    print(f"    [{cells}],")
print("];\n")
lps = array(common, "g_kuiCabacRangeLps"); assert len(lps) == 256
print("/// Table 9-44: rangeTabLPS by pStateIdx and (codIRange >> 6) & 3.")
print("pub static RANGE_LPS: [[u8; 4]; 64] = [")
for s in range(64):
    print("    [" + ", ".join(map(str, lps[s*4:s*4+4])) + "],")
print("];\n")
tr = array(common, "g_kuiStateTransTable"); assert len(tr) == 128
print("/// Table 9-45: next pStateIdx after an LPS and after an MPS.")
print("pub static STATE_TRANS: [[u8; 2]; 64] = [")
for s in range(0, 128, 16):
    print("    " + ", ".join(f"[{tr[k]}, {tr[k+1]}]" for k in range(s, s+16, 2)) + ",")
print("];\n")
emit("ZIGZAG_4X4", "u8", array(basis, "g_kuiZigzagScan"), 16, "Frame zig-zag scan of a 4x4 block: scan position to raster index.")
emit("ZIGZAG_8X8", "u8", array(basis, "g_kuiZigzagScan8x8"), 16, "Frame zig-zag scan of an 8x8 block.")
emit("SIG_CTX_8X8", "u8", array(basis, "g_kuiIdx2CtxSignificantCoeffFlag8x8"), 16, "Table 9-43: ctxIdxInc of significant_coeff_flag in an 8x8 block, frame coded.")
emit("LAST_CTX_8X8", "u8", array(basis, "g_kuiIdx2CtxLastSignificantCoeffFlag8x8"), 16, "Table 9-43: ctxIdxInc of last_significant_coeff_flag in an 8x8 block.")
a = array(deblock, "g_kuiAlphaTable")[12:12+52]
b = array(deblock, "g_kiBetaTable")[12:12+52]
t = array(deblock, "g_kiTc0Table")
assert len(a) == 52 and len(b) == 52 and len(t) == 76 * 4, (len(a), len(b), len(t))
emit("ALPHA", "u8", a, 16, "Table 8-16: alpha' by indexA.")
emit("BETA", "u8", b, 16, "Table 8-16: beta' by indexB.")
print("/// Table 8-17: tC0' by indexA for bS 1, 2 and 3.")
print("pub static TC0: [[u8; 3]; 52] = [")
for q in range(52):
    r = t[(q+12)*4:(q+12)*4+4]
    print(f"    [{r[1]}, {r[2]}, {r[3]}],  // {r}")
print("];")
