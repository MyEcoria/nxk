#!/usr/bin/env bash
# Populates a FAT32 microSD card (already mounted) for Switch bring-up.
# Usage: ./mksd-hw.sh /mount/point
# Layout: bootloader/ (hekate+Nx), bootloader/hekate_ipl.ini,
# bootloader/payloads/nxk.bin (stage-2 marker, not bootable yet),
# /bin/* + /etc/motd (same userland as tools/mksd.py puts in sd.img).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
cache="${NXK_CACHE:-$HOME/.cache/nxk}"

sd="${1:?usage: mksd-hw.sh /mount/point}"
[ -d "$sd" ] || { echo "[mksd-hw] not a directory: $sd" >&2; exit 1; }

nyx="$here/dl/hekate_nyx.zip"
[ -s "$nyx" ] || { echo "[mksd-hw] missing $nyx (run ./fetch.sh)" >&2; exit 1; }

echo "[mksd-hw] bootloader files -> $sd"
if [ -d "$sd/bootloader/sys" ]; then
    echo "[mksd-hw] existing bootloader/ kept (Nyx pack already installed)"
else
    unzip -oq "$nyx" -d "$sd" -x 'bootloader/ini/*'
fi
if [ -f "$sd/bootloader/hekate_ipl.ini" ]; then
    if ! grep -q "nxk (TODO)" "$sd/bootloader/hekate_ipl.ini"; then
        [ -f "$sd/bootloader/hekate_ipl.ini.bak-nxk" ] || \
            cp "$sd/bootloader/hekate_ipl.ini" "$sd/bootloader/hekate_ipl.ini.bak-nxk"
        printf '\n' >> "$sd/bootloader/hekate_ipl.ini"
        sed -n '/^# STAGE 2/,$p' "$here/hekate_ipl.ini" >> "$sd/bootloader/hekate_ipl.ini"
        echo "[mksd-hw] appended nxk marker to hekate_ipl.ini (backup: .bak-nxk)"
    else
        echo "[mksd-hw] nxk marker already in hekate_ipl.ini"
    fi
else
    cp "$here/hekate_ipl.ini" "$sd/bootloader/hekate_ipl.ini"
fi

"$root/tools/build.sh" >&2
OBJCOPY="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$root/build/nxk.elf" "$root/build/nxk.bin"
mkdir -p "$sd/bootloader/payloads"
cp "$root/build/nxk.bin" "$sd/bootloader/payloads/nxk.bin"
echo "[mksd-hw] nxk.bin $(stat -c%s "$root/build/nxk.bin") bytes (stage-2 marker)"

mkdir -p "$sd/bin"
bindir="$cache/target-user/aarch64-unknown-none/release"
for prog in hello ktest true fptest crash bench ipcbench; do
    [ -x "$bindir/$prog" ] && cp "$bindir/$prog" "$sd/bin/$prog"
done
appdir="$cache/target-apps/aarch64-unknown-nxk/release"
for prog in hello-std stdtest; do
    [ -x "$appdir/$prog" ] && cp "$appdir/$prog" "$sd/bin/$prog"
done
printf 'Bienvenue sur nxk : ce fichier vient de la carte SD (FAT32).\n' > "$sd/motd-nxk.txt"
sync
echo "[mksd-hw] SD ready:"
ls "$sd" | head
du -sh "$sd/bootloader" "$sd/bin"
