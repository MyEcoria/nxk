#!/usr/bin/env bash
# Build nxk and run it on the emulated Tegra X1 (tegra_qemu).
# UART-A console on the terminal: Ctrl-A then X to quit.
set -euo pipefail

ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu not found: $qemu (QEMU var for the path)" >&2; exit 1; }

"$ici/tools/build.sh" >&2

exec "$qemu" \
    -machine tegrax1 -m 4G \
    -display none \
    -device "loader,file=$ici/build/nxk.elf" \
    -global driver=tegra.evp,property=cpu-reset-vector,value=0x80000000 \
    -chardev stdio,id=uarta,signal=off -serial chardev:uarta \
    "$@"
