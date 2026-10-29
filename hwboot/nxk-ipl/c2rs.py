#!/usr/bin/env python3
"""Transliteration of hekate's _sdram_config_t210{,b01} to Rust, as used
for src/sdram_seq.rs (kept to redo it on a newer hekate).

    c2rs.py refs/hekate/bdk/mem/sdram.c FIRST LAST > seq.rs

FIRST..LAST: the line range of the two functions. Register macros become
rd/wr, `params->` becomes `p.`; the PLLM lock wait (a goto) is rewritten
by hand afterwards as a labeled block, the only line left as FIXME.
"""
import re, sys

BASES = {"EMC_CH0": "EMC0_BASE", "EMC_CH1": "EMC1_BASE", "EMC": "EMC_BASE", "MC": "MC_BASE",
         "CLOCK": "CLOCK_BASE", "PMC": "PMC_BASE", "APB_MISC": "APB_MISC_BASE",
         "SYSREG": "SYSREG_BASE", "FUSE": "FUSE_BASE"}
MAC = "|".join(sorted(BASES, key=len, reverse=True))


def expr(e):
    e = e.strip()
    e = re.sub(r"(\b0[xX][0-9a-fA-F]+|\b\d+)[uU]\b", r"\1", e)
    e = e.replace("params->", "p.")
    e = re.sub(r"\bBIT\(([^()]+)\)", r"(1 << \1)", e)
    e = re.sub(rf"\b({MAC})\(([A-Z0-9_]+)\)", lambda m: f"rd({BASES[m.group(1)]} + {m.group(2)})", e)
    e = e.replace("get_tmr_us()", "now_us()")
    e = e.replace("~", "!")
    return e


def cond(c):
    c = c.strip().replace("params->", "p.")
    m = re.fullmatch(r"!\((.*)\)", c)
    if m:
        return f"({expr(m.group(1))}) == 0"
    m = re.fullmatch(r"!(p\.\w+|params->\w+)", c)
    if m:
        return f"{expr(m.group(1))} == 0"
    if re.search(r"==|!=|>=|<=|<|>", c):
        return expr(c)
    return f"({expr(c)}) != 0"


def stmt(s):
    s = s.strip()
    comment = ""
    if "//" in s:
        s, comment = s.split("//", 1)
        s = s.strip()
        comment = " //" + comment
    if not s:
        return comment.strip()
    m = re.fullmatch(rf"({MAC})\(([A-Z0-9_]+)\)\s*(\|=|&=|=)\s*(.+);", s)
    if m:
        base, reg, op, val = BASES[m.group(1)], m.group(2), m.group(3), expr(m.group(4))
        a = f"{base} + {reg}"
        if op == "=":
            return f"wr({a}, {val});{comment}"
        o = "|" if op == "|=" else "&"
        return f"wr({a}, rd({a}) {o} ({val}));{comment}"
    m = re.fullmatch(r"\*\(vu32 \*\)\(?(.+?)\)?\s*=\s*(.+);", s)
    if m:
        return f"wr({expr(m.group(1))}, {expr(m.group(2))});{comment}"
    m = re.fullmatch(rf"\(void\)({MAC})\(([A-Z0-9_]+)\);", s)
    if m:
        return f"let _ = rd({BASES[m.group(1)]} + {m.group(2)});{comment}"
    m = re.fullmatch(r"u32 (\w+)\s*=\s*(.+);", s)
    if m:
        return f"let mut {m.group(1)}: u32 = {expr(m.group(2))};{comment}"
    m = re.fullmatch(r"usleep\((.+)\);", s)
    if m:
        return f"usleep({expr(m.group(1))});{comment}"
    m = re.fullmatch(r"(\w+)\s*(\|=|&=|=)\s*(.+);", s)
    if m:
        return f"{m.group(1)} {m.group(2)} {expr(m.group(3))};{comment}"
    return f"/* FIXME */ {s}{comment}"


def main():
    lines = open(sys.argv[1]).read().splitlines()[int(sys.argv[2]) - 1:int(sys.argv[3])]
    out, i, pending = [], 0, 0
    while i < len(lines):
        raw = lines[i]
        ind = re.match(r"\s*", raw).group(0).replace("\t", "    ")
        s = raw.strip()
        i += 1
        if not s:
            out.append("")
            continue
        m = re.match(r"static void _sdram_config_(t210|t210b01)\(", s)
        if m:
            t = "ParamsT210" if m.group(1) == "t210" else "ParamsT210b01"
            out.append(f"pub fn config_{m.group(1)}(p: &{t}) {{")
            i += 1  # the brace line
            continue
        if s in ("{",):
            continue  # braces are emitted by the if/else/while lines
        if s == "}":
            out.append(ind + "}")
            continue
        m = re.fullmatch(r"(if|while) \((.*)\)", s)
        if m:
            out.append(f"{ind}{m.group(1)} {cond(m.group(2))} {{")
            # single statement without braces
            nxt = lines[i].strip() if i < len(lines) else ""
            if nxt != "{":
                out.append(ind + "    " + stmt(lines[i]))
                out.append(ind + "}")
                i += 1
            continue
        if s == "else":
            if out and out[-1].strip() == "}":
                out[-1] = out[-1] + " else {"
            nxt = lines[i].strip()
            if nxt != "{":
                out.append(ind + "    " + stmt(lines[i]))
                out.append(ind + "}")
                i += 1
            continue
        if s.startswith("//"):
            out.append(ind + s)
            continue
        out.append(ind + stmt(s))
    print("\n".join(out))


main()
