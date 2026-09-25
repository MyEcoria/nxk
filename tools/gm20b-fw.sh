#!/usr/bin/env bash
# GM20B GPU firmware for the `gpu` service, taken from the PC's
# linux-firmware package (NVIDIA's redistributable set that nouveau
# loads): decompressed into firmware/nvidia/gm20b (git ignores
# firmware/), which `tools/mksd.py --firmware firmware` and
# `hwboot/mksd-hw.sh MOUNT firmware` copy to /firmware/nvidia on the card.
#
#   tools/gm20b-fw.sh [SOURCE] [DEST]
#   (defaults: /lib/firmware/nvidia/gm20b, firmware/nvidia/gm20b)
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="${1:-/lib/firmware/nvidia/gm20b}"
dst="${2:-$root/firmware/nvidia/gm20b}"

# The ACR HS loader and its bootloader, FECS (secure, through the ACR),
# GPCCS (loaded directly) and the netlists. The PMU RTOS is not used.
files=(
    acr/bl.bin acr/ucode_load.bin
    gr/fecs_bl.bin gr/fecs_inst.bin gr/fecs_data.bin gr/fecs_sig.bin
    gr/gpccs_inst.bin gr/gpccs_data.bin
    gr/sw_ctx.bin gr/sw_nonctx.bin gr/sw_bundle_init.bin gr/sw_method_init.bin
)

for f in "${files[@]}"; do
    mkdir -p "$dst/$(dirname "$f")"
    # Distributions ship them plain, zstd- or xz-compressed; some are
    # symlinks into gm200/ (readlink -f follows them).
    if [ -e "$src/$f.zst" ]; then
        zstd -q -d -f "$(readlink -f "$src/$f.zst")" -o "$dst/$f"
    elif [ -e "$src/$f.xz" ]; then
        xz -d -c "$(readlink -f "$src/$f.xz")" > "$dst/$f"
    elif [ -e "$src/$f" ]; then
        cp "$(readlink -f "$src/$f")" "$dst/$f"
    else
        echo "[gm20b-fw] missing $src/$f (install linux-firmware)" >&2
        exit 1
    fi
done
echo "[gm20b-fw] ${#files[@]} files -> $dst"
