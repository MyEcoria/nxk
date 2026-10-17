#!/usr/bin/env bash
# Build nxk: user programs -> boot image -> kernel (build dirs in ~/.cache/nxk).
set -euo pipefail

ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache="${NXK_CACHE:-$HOME/.cache/nxk}"
export PATH="$HOME/.cargo/bin:$PATH"

progs=(init console shell hello ipcbench crash fptest bench true ktest clk i2c power fan cpufreq sd fs display input joycon audio usb usbcon net nettest wlan wifi gpu)
# Ordinary (std) programs, apps/ dir.
apps=(hello-std stdtest)

echo "== userland"
# Screen font, converted from a system console font.
python3 "$ici/tools/mkfont.py" -o "$ici/build/font.bin"
# Proportional antialiased UI font atlas.
python3 "$ici/tools/mkuifont.py" -o "$ici/build/uifont.bin"
# Paletted RGB565 game cover artwork.
python3 "$ici/tools/mkcovers.py" -o "$ici/build/covers.bin"
# Boot screen logo of the display service.
python3 "$ici/tools/mklogo.py" --raw "$ici/build/logo.bin"
# cargo reads .cargo/config.toml from cwd: enter it.
(cd "$ici/user" && CARGO_TARGET_DIR="$cache/target-user" cargo build --release)
bins=()
for p in "${progs[@]}"; do bins+=("$cache/target-user/aarch64-unknown-none/release/$p"); done

echo "== std programs"
# "nxk" toolchain: stable Rust with ported standard library.
python3 "$ici/tools/mkstd.py"
(cd "$ici/apps" && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$cache/target-apps" cargo build --release)
for p in "${apps[@]}"; do bins+=("$cache/target-apps/aarch64-unknown-nxk/release/$p"); done

echo "== games"
# Games (jeux/ dir) are std programs too, but live on the SD card
# (/games/NAME), not in the boot image: tools/mksd.py installs them.
(cd "$ici/jeux" && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$cache/target-apps" cargo build --release)

python3 "$ici/tools/mkbootfs.py" -o "$ici/build/bootfs.img" "${bins[@]}"

echo "== kernel"
# NXK_FEATURES=idle-poll: idle cores spin (L4T comparison).
(cd "$ici/kernel" && CARGO_TARGET_DIR="$cache/target-kernel" cargo build --release ${NXK_FEATURES:+--features "$NXK_FEATURES"})
ln -sf "$cache/target-kernel/aarch64-unknown-none-softfloat/release/nxk" "$ici/build/nxk.elf"
echo "== ready: build/nxk.elf"
