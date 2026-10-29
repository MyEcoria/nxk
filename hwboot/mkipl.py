#!/usr/bin/env python3
"""Build nxk-ipl, nxk's RCM payload for the BPMP -> nxk-ipl.bin.

    mkipl.py [-o hwboot/dl/nxk-ipl.bin]

Thumb (ARMv4T) target, tier 3: core is built from rust-src under
RUSTC_BOOTSTRAP, as for the std programs. Fails if the image passes
hekate's IPL budget (128 KiB from 0x40008000).
"""
import argparse, os, subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.join(HERE, "nxk-ipl")
CACHE = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
OBJCOPY = os.path.expanduser("~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                             "/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy")
BUDGET = 128 << 10

ap = argparse.ArgumentParser()
ap.add_argument("-o", default=os.path.join(HERE, "dl", "nxk-ipl.bin"))
a = ap.parse_args()

env = dict(os.environ, RUSTC_BOOTSTRAP="1",
           CARGO_TARGET_DIR=os.path.join(CACHE, "target-ipl"),
           PATH=os.path.expanduser("~/.cargo/bin") + ":" + os.environ.get("PATH", ""))
subprocess.run(["cargo", "build", "--release"], cwd=CRATE, env=env, check=True)
elf = os.path.join(CACHE, "target-ipl", "thumbv4t-none-eabi", "release", "nxk-ipl")
os.makedirs(os.path.dirname(a.o), exist_ok=True)
subprocess.run([OBJCOPY, "-O", "binary", elf, a.o], check=True)
size = os.path.getsize(a.o)
print(f"nxk-ipl: {size} bytes ({size * 100 // BUDGET}% of 128 KiB) -> {a.o}")
if size > BUDGET:
    raise SystemExit("nxk-ipl: over hekate's 128 KiB IPL budget")
