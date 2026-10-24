#!/usr/bin/env bash
# Populate a mounted FAT32 microSD for Switch bring-up (hekate + /bin userland).
# Usage: ./mksd-hw.sh /mount/point [firmware-dir]
# firmware-dir: Wi-Fi blobs copied to /firmware/brcm (brcmfmac4356-pcie.*),
# its nvidia/ tree (GPU, tools/gm20b-fw.sh) to /firmware/nvidia.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
cache="${NXK_CACHE:-$HOME/.cache/nxk}"

sd="${1:?usage: mksd-hw.sh /mount/point [firmware-dir]}"
[ -d "$sd" ] || { echo "[mksd-hw] not a directory: $sd" >&2; exit 1; }
fw="${2:-${NXK_FIRMWARE:-}}"
[ -z "$fw" ] || [ -d "$fw" ] || { echo "[mksd-hw] not a directory: $fw" >&2; exit 1; }

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
for prog in hello-std stdtest speedtest; do
    [ -x "$appdir/$prog" ] && cp "$appdir/$prog" "$sd/bin/$prog"
done
# Games (jeux/): /games/NAME/NAME, its game.txt and cover, for the library.
for dir in "$root"/jeux/*/; do
    name="$(basename "$dir")"
    if [ -x "$appdir/$name" ] && [ -f "$dir/game.txt" ]; then
        mkdir -p "$sd/games/$name"
        cp "$appdir/$name" "$sd/games/$name/$name"
        cp "$dir/game.txt" "$sd/games/$name/game.txt"
        cover="$root/build/covers/$name.bin"
        if [ -f "$cover" ]; then cp "$cover" "$sd/games/$name/cover.bin"; fi
    fi
done
# Game catalogs (services/): /services/NAME/NAME and its service.txt.
for dir in "$root"/services/*/; do
    name="$(basename "$dir")"
    if [ -x "$appdir/$name" ] && [ -f "$dir/service.txt" ]; then
        mkdir -p "$sd/services/$name"
        cp "$appdir/$name" "$sd/services/$name/$name"
        cp "$dir/service.txt" "$sd/services/$name/service.txt"
    fi
done
printf 'Welcome to nxk: this file comes from the SD card (FAT32).\n' > "$sd/motd-nxk.txt"

# Wi-Fi firmware and calibration data, GPU firmware: never in the repo.
if [ -n "$fw" ]; then
    mkdir -p "$sd/firmware/brcm"
    find "$fw" -maxdepth 1 -type f -exec cp {} "$sd/firmware/brcm/" \;
    echo "[mksd-hw] Wi-Fi firmware -> $sd/firmware/brcm ($(find "$fw" -maxdepth 1 -type f | wc -l) files)"
    if [ -d "$fw/nvidia" ]; then
        mkdir -p "$sd/firmware"
        cp -r "$fw/nvidia" "$sd/firmware/"
        echo "[mksd-hw] GPU firmware -> $sd/firmware/nvidia ($(find "$fw/nvidia" -type f | wc -l) files)"
    fi
fi

echo "[mksd-hw] nxk L4T entry -> $sd"
python3 "$here/mkbl31.py" -o "$here/dl/bl31.bin"
mkdir -p "$sd/nxk"
cp "$here/dl/bl31.bin" "$sd/nxk/bl31.bin"
cp "$root/build/nxk.bin" "$sd/nxk/bl33.bin"
mkdir -p "$sd/bootloader/ini"
cp "$here/nxk.ini" "$sd/bootloader/ini/nxk.ini"
if [ ! -d "$sd/bootloader/sys/l4t" ]; then
    echo "[mksd-hw] firmware L4T hekate (BPMP/SC7/Minerva)"
    unzip -oq "$nyx" -d "$sd" 'bootloader/sys/l4t/*' 'bootloader/sys/libsys_*.bso'
else
    echo "[mksd-hw] L4T firmware already present"
fi
sync
echo "[mksd-hw] SD ready:"
ls "$sd" | head
du -sh "$sd/bootloader" "$sd/bin"
