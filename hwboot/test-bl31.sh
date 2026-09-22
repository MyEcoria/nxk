#!/usr/bin/env bash
# Test de bout en bout du moniteur nxk-bl31 sous QEMU (meme binaire que sur
# le materiel) : moniteur a 0xFFF00000, nxk.bin a 0xAA000000.
#  1. Sans ecran, reset sur le moniteur. Succes = banniere nxk "entree en
#     EL2", 1 seul coeur (pas de PSCI v1), puis invite du shell.
#  2. Ecran laisse allume par hekate (fakehekate.py : fenetre A sur un faux
#     logo). Succes = logo efface, lignes du moniteur puis du noyau lisibles
#     dans l'image jusqu'au lancement de init.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu introuvable : $qemu" >&2; exit 1; }

[ -e "$root/build/nxk.elf" ] || { echo "lancez tools/build.sh d'abord" >&2; exit 1; }
OBJCOPY="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$root/build/nxk.elf" "$root/build/nxk.bin"

python3 "$here/mkbl31.py" -o "$here/dl/bl31-test.bin"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Lance QEMU jusqu'a l'invite du shell (60 s au plus), vide l'image de
# l'ecran de hekate dans $tmp/fb.raw, puis arrete. $1 : vecteur de reset.
run() {
    local reset="$1"
    shift
    rm -f "$tmp/uart.log" "$tmp/fb.raw"
    "$qemu" -machine tegrax1 -m 4G -display none \
        -device "loader,file=$here/dl/bl31-test.bin,addr=0xFFF00000,force-raw=on" \
        -device "loader,file=$root/build/nxk.bin,addr=0xAA000000,force-raw=on" \
        "$@" \
        -global "driver=tegra.evp,property=cpu-reset-vector,value=$reset" \
        -serial "file:$tmp/uart.log" -serial null -serial null -serial null \
        -monitor "unix:$tmp/mon,server,nowait" >"$tmp/qemu.log" 2>&1 &
    local pid=$!
    for _ in $(seq 60); do
        sleep 1
        grep -q "nxk> " "$tmp/uart.log" 2>/dev/null && break
    done
    python3 - "$tmp" <<'PY'
import socket, sys, time
m = socket.socket(socket.AF_UNIX)
m.connect(sys.argv[1] + "/mon")
for c in [f'pmemsave 0xF5A00000 3686400 "{sys.argv[1]}/fb.raw"', "quit"]:
    m.sendall((c + "\n").encode())
    time.sleep(1.5)
PY
    wait "$pid" || true
}

ok=1
check() {
    local f="$1"
    shift
    for motif in "$@"; do
        grep -q -- "$motif" "$f" || { echo "MANQUANT : $motif"; ok=0; }
    done
}

echo "== sans ecran"
run 0xFFF00000
check "$tmp/uart.log" "nxk-bl31.*saut en EL2" "entree en EL2" "sans PSCI : un seul coeur" "nxk> "
tr -d '\r' <"$tmp/uart.log" | grep -E "nxk-bl31|entree en EL2|coeurs en ligne" | head -12

echo "== ecran laisse par hekate"
python3 "$here/fakehekate.py" amorce "$tmp/amorce.bin"
python3 "$here/fakehekate.py" logo "$tmp/logo.raw"
run 0xF0000000 \
    -device "loader,file=$tmp/amorce.bin,addr=0xF0000000,force-raw=on" \
    -device "loader,file=$tmp/logo.raw,addr=0xF5A00000,force-raw=on"
check "$tmp/uart.log" "nxk> "
python3 "$here/fakehekate.py" lire "$tmp/fb.raw" >"$tmp/fb.txt"
check "$tmp/fb.txt" "^\[nxk-bl31\] moniteur EL3" "^\[nxk-bl31\] saut en EL2" "micro-noyau Tegra X1" \
    "^\[ecran\] messages recopies" "lancement de init" "^pixels hors texte : 0$"
grep -E "nxk-bl31\] (moniteur|saut)|micro-noyau|\[ecran\]|lancement de init|pixels hors texte" "$tmp/fb.txt"

[ "$ok" = 1 ] && echo "TEST-BL31 OK" || { echo "TEST-BL31 ECHEC"; exit 1; }
