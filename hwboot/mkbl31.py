#!/usr/bin/env python3
"""Construit le moniteur nxk-bl31 -> bl31.bin (charge utile hekate L4T).

    mkbl31.py -o hwboot/dl/bl31.bin [--nxk build/nxk.bin]

Taille de nxk.bin injectee a la compilation (NXK_BL33_SIZE) : le moniteur
recopie exactement ces octets de 0xAA000000 vers 0x80000000.
"""
import argparse, os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CRATE = os.path.join(HERE, "nxk-bl31")
CACHE = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
OBJCOPY = os.path.expanduser("~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                             "/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy")

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True, help="bl31.bin de sortie")
ap.add_argument("--nxk", default=os.path.join(ROOT, "build", "nxk.bin"),
                help="image brute de nxk (defaut: build/nxk.bin)")
a = ap.parse_args()

if not os.path.exists(a.nxk):
    # ECD: build/nxk.bin n'existe pas encore ? on le derive du ELF.
    elf = os.path.join(ROOT, "build", "nxk.elf")
    if not os.path.exists(elf):
        raise SystemExit("ni nxk.bin ni nxk.elf : lancez tools/build.sh d'abord")
    subprocess.run([OBJCOPY, "-O", "binary", elf, a.nxk], check=True)

size = os.path.getsize(a.nxk)
print(f"bl33 (nxk.bin) : {size} octets")
if size == 0 or size > 8 << 20:
    raise SystemExit("taille bl33 invraisemblable")

env = dict(os.environ,
           CARGO_TARGET_DIR=os.path.join(CACHE, "target-bl31"),
           NXK_BL33_SIZE=str(size),
           PATH=os.path.expanduser("~/.cargo/bin") + ":" + os.environ.get("PATH", ""))
subprocess.run(["cargo", "build", "--release"], cwd=CRATE, env=env, check=True)
elf = os.path.join(CACHE, "target-bl31", "aarch64-unknown-none-softfloat",
                   "release", "nxk-bl31")
subprocess.run([OBJCOPY, "-O", "binary", elf, a.o], check=True)
print(f"bl31 : {os.path.getsize(a.o)} octets -> {a.o} "
      f"(charge a 0xFFF00000, TZDRAM 1 Mo : OK)"
      if os.path.getsize(a.o) < 1 << 20 else "TROP GROS pour TZDRAM")
