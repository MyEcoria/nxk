#!/usr/bin/env bash
# Test de bout en bout du moniteur nxk-bl31 sous QEMU : le moniteur est
# depose a 0xFFF00000, nxk.bin a 0xAA000000, reset sur le moniteur.
# Succes = banniere nxk "entree en EL2", 1 seul coeur (pas de PSCI v1),
# puis invite du shell. Meme binaire bl31 que sur le materiel.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu introuvable : $qemu" >&2; exit 1; }

[ -e "$root/build/nxk.elf" ] || { echo "lancez tools/build.sh d'abord" >&2; exit 1; }
OBJCOPY="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$root/build/nxk.elf" "$root/build/nxk.bin"

python3 "$here/mkbl31.py" -o "$here/dl/bl31-test.bin"

log="$(mktemp)"
timeout 60 "$qemu" \
    -machine tegrax1 -m 4G -display none \
    -device "loader,file=$here/dl/bl31-test.bin,addr=0xFFF00000,force-raw=on" \
    -device "loader,file=$root/build/nxk.bin,addr=0xAA000000,force-raw=on" \
    -global driver=tegra.evp,property=cpu-reset-vector,value=0xFFF00000 \
    -serial stdio -serial null -serial null -serial null >"$log" 2>&1 || true

ok=1
for motif in "nxk-bl31.*saut en EL2" "entree en EL2" "sans PSCI : un seul coeur" "nxk> "; do
    grep -q "$motif" "$log" || { echo "MANQUANT : $motif"; ok=0; }
done
grep -E "nxk-bl31|entree en EL2|coeurs en ligne" "$log" | head -12
rm -f "$log"
[ "$ok" = 1 ] && echo "TEST-BL31 OK" || { echo "TEST-BL31 ECHEC"; exit 1; }
