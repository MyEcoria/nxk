#!/usr/bin/env bash
# Construit nxk et le lance sur le Tegra X1 emule (tegra_qemu).
# La console UART-A est branchee sur le terminal : Ctrl-A puis X pour quitter.
set -euo pipefail

ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu introuvable : $qemu (variable QEMU pour le chemin)" >&2; exit 1; }

"$ici/tools/build.sh" >&2

exec "$qemu" \
    -machine tegrax1 -m 4G \
    -display none \
    -device "loader,file=$ici/build/nxk.elf" \
    -global driver=tegra.evp,property=cpu-reset-vector,value=0x80000000 \
    -chardev stdio,id=uarta,signal=off -serial chardev:uarta \
    "$@"
