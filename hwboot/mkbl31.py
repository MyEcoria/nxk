#!/usr/bin/env python3
"""Build the nxk-bl31 monitor -> bl31.bin (hekate L4T payload).

    mkbl31.py -o hwboot/dl/bl31.bin [--nxk build/nxk.bin]

nxk.bin size is baked in at compile time (NXK_BL33_SIZE): the monitor copies
exactly those bytes from 0xAA000000 to 0x80000000.
"""
import argparse, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CRATE = os.path.join(HERE, "nxk-bl31")
CACHE = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
OBJCOPY = os.path.expanduser("~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                             "/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy")

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True, help="output bl31.bin")
ap.add_argument("--nxk", default=os.path.join(ROOT, "build", "nxk.bin"),
                help="raw nxk image (default: build/nxk.bin)")
a = ap.parse_args()

if not os.path.exists(a.nxk):
    # Fallback: derive build/nxk.bin from the ELF if missing.
    elf = os.path.join(ROOT, "build", "nxk.elf")
    if not os.path.exists(elf):
        raise SystemExit("neither nxk.bin nor nxk.elf: run tools/build.sh first")
    subprocess.run([OBJCOPY, "-O", "binary", elf, a.nxk], check=True)

size = os.path.getsize(a.nxk)
print(f"bl33 (nxk.bin): {size} bytes")
if size == 0 or size > 8 << 20:
    raise SystemExit("implausible bl33 size")

env = dict(os.environ,
           CARGO_TARGET_DIR=os.path.join(CACHE, "target-bl31"),
           NXK_BL33_SIZE=str(size),
           PATH=os.path.expanduser("~/.cargo/bin") + ":" + os.environ.get("PATH", ""))
subprocess.run(["cargo", "build", "--release"], cwd=CRATE, env=env, check=True)
elf = os.path.join(CACHE, "target-bl31", "aarch64-unknown-none-softfloat",
                   "release", "nxk-bl31")
subprocess.run([OBJCOPY, "-O", "binary", elf, a.o], check=True)
print(f"bl31: {os.path.getsize(a.o)} bytes -> {a.o} "
      f"(loaded at 0xFFF00000, 1 MiB TZDRAM: OK)"
      if os.path.getsize(a.o) < 1 << 20 else "TOO BIG for TZDRAM")
