#!/usr/bin/env bash
# End-to-end test of nxk-bl31 under QEMU (same binary as hardware).
# 1. No screen: nxk banner, PSCI 1.0, 4 cores via CPU_ON, shell prompt. 2. Hekate-lit screen: logo cleared, monitor + kernel lines readable.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu not found: $qemu" >&2; exit 1; }

[ -e "$root/build/nxk.elf" ] || { echo "run tools/build.sh first" >&2; exit 1; }
OBJCOPY="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$root/build/nxk.elf" "$root/build/nxk.bin"

python3 "$here/mkbl31.py" -o "$here/dl/bl31-test.bin"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Run QEMU until the shell prompt (60 s max), dump the hekate screen image
# to $tmp/fb.raw, then stop. $1: reset vector.
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
        grep -q -- "$motif" "$f" || { echo "MISSING: $motif"; ok=0; }
    done
}

echo "== no screen"
run 0xFFF00000
check "$tmp/uart.log" "nxk-bl31.*jumping to EL2" "entered in EL2" "by a monitor: PSCI 1.0" "4/4 cores online" "nxk> "
tr -d '\r' <"$tmp/uart.log" | grep -E "nxk-bl31|entered in EL2|cores online" | head -12

echo "== screen left on by hekate"
python3 "$here/fakehekate.py" amorce "$tmp/amorce.bin"
python3 "$here/fakehekate.py" logo "$tmp/logo.raw"
run 0xF0000000 \
    -device "loader,file=$tmp/amorce.bin,addr=0xF0000000,force-raw=on" \
    -device "loader,file=$tmp/logo.raw,addr=0xF5A00000,force-raw=on"
check "$tmp/uart.log" "4/4 cores online" "nxk> "
python3 "$here/fakehekate.py" lire "$tmp/fb.raw" >"$tmp/fb.txt"
check "$tmp/fb.txt" "^\[nxk-bl31\] EL3 monitor" "^\[nxk-bl31\] jumping to EL2" "Tegra X1 microkernel" \
    "^\[screen\] messages mirrored" "launching init" "^pixels outside text: 0$"
grep -E "nxk-bl31\] (EL3 monitor|jumping)|microkernel|\[screen\]|launching init|pixels outside text" "$tmp/fb.txt"

[ "$ok" = 1 ] && echo "TEST-BL31 OK" || { echo "TEST-BL31 FAIL"; exit 1; }
