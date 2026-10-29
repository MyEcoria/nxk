#!/bin/sh
# Build the register-trace oracle: hekate's Minerva switch code (C) with its
# register accesses recorded, to compare against the Rust port (emc/tests).
#
#   emc/oracle/build.sh HEKATE_DIR OUT_BINARY
#
# HEKATE_DIR is a hekate checkout (refs/hekate); its sources are copied to a
# scratch directory and patched there: nothing is added to the repository
# and refs/ is never compiled in place (AGENTS.md section 6).
set -eu
hek=$(cd "$1" && pwd)
out=$(mkdir -p "$(dirname "$2")" && cd "$(dirname "$2")" && pwd)/$(basename "$2")
here=$(cd "$(dirname "$0")" && pwd)
mod=$hek/modules/hekate_libsys_minerva
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cp "$mod"/*.c "$mod"/*.h "$tmp"/
cp "$here"/oracle_regs.h "$here"/oracle.cpp "$tmp"/
: > "$tmp/module.h"
echo 'typedef struct { int dummy; } bdk_params_t;' > "$tmp/bdkp.h"
python3 - "$tmp" <<'PY'
import re, sys
d = sys.argv[1]
# Register macros -> the recording bus.
p = d + "/mtc.h"
s = open(p).read()
a = s.index("/* Address bases")
b = s.index("/* End of addresses and access macros */")
s = s[:a] + '#include "oracle_regs.h"\n' + s[b:]
open(p, "w").write(s)
# C++ needs its own bool.
p = d + "/types.h"
s = open(p).read()
s = s.replace("typedef int bool;", "").replace("#define true  1", "").replace("#define false 0", "")
s = s.replace("#define NULL ((void *)0)", "")
open(p, "w").write(s)
# Delays and register read-backs go through the recorder.
p = d + "/sys_sdrammtc.c"
s = open(p).read()
s = re.sub(r"static void _usleep\(u32 microseconds\)\n\{.*?\n\}",
           "void trace_usleep(unsigned);\nstatic void _usleep(u32 microseconds)\n{\n\ttrace_usleep(microseconds);\n}",
           s, count=1, flags=re.S)
s = re.sub(r"\(void\)(EMC|MC|CLOCK)\(", r"(void)(u32)\1(", s)
s = s.replace("#include <module.h>", '#include "bdkp.h"')
# Divisions by zero give what hekate's ARM build gives (libgcc's EABI
# __aeabi_uidiv: 0xFFFFFFFF, or 0 for 0 / 0) instead of the PC's trap:
# the clock tree code divides by oscillator counts read back as 0.
s = s.replace('#include "bdkp.h"', '#include "bdkp.h"\nstatic inline unsigned _udiv(unsigned a, unsigned b) { return b ? a / b : (a ? 0xFFFFFFFFu : 0); }', 1)
s = re.sub(r"cval = tval / \((.*?)\);", r"cval = _udiv(tval, \1);", s)
s = re.sub(r"(dst_emc_entry->ptfv_list\.ptfv_dqsosc_movavg_\w+) / (dst_emc_entry->ptfv_list\.ptfv_(?:dvfs|write)_samples);",
           r"_udiv(\1, \2);", s)
s = re.sub(r"\(100 \* cval \+ (.*?)\)\s*/ \((dst_emc_entry->ptfv_list\.ptfv_movavg_weight \+ 1)\)",
           r"_udiv(100 * cval + \1, \2)", s, flags=re.S)
open(p, "w").write(s)
PY
g++ -std=gnu++17 -O1 -fpermissive -w -I"$tmp" -x c++ "$tmp/oracle.cpp" -o "$out"
