#!/usr/bin/env bash
# The whole boot chain in the emulator, from the RCM payload on the BPMP:
# PAYLOAD (default hekate, autobooting its [nxk] L4T entry) -> nxk-bl31 ->
# nxk -> shell. hekate's run is the reference nxk-ipl is compared with.
# Usage: oracle.sh [-m tegrax1|tegrax1plus] [-p PAYLOAD] [-r DRAMID] [-t TRACE] [-s SECS]
#                  [-d SHOT.ppm] [-u UART.log]
#   -r: the DRAM id in the emulated fuses (hekate fuse_read_dramid, default 0).
#   -t: every BPMP and CPU register access (QEMU memory_region_ops) to TRACE.
#   -d: the screen at the end. -u: UART-A's whole output.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
qemu="${QEMU:-$HOME/.cache/tegra_qemu/build-mc/qemu-system-aarch64}"
[ -x "$qemu" ] || { echo "qemu not found: $qemu" >&2; exit 1; }

machine=tegrax1
payload="$here/dl/hekate_ctcaer_6.5.4.bin"
trace=""
shot=""
ulog=""
secs=120
dramid=0
while getopts "m:p:r:t:s:d:u:" o; do
    case "$o" in
    m) machine="$OPTARG" ;;
    p) payload="$OPTARG" ;;
    t) trace="$OPTARG" ;;
    s) secs="$OPTARG" ;;
    r) dramid="$OPTARG" ;;
    d) shot="$(realpath -m "$OPTARG")" ;;
    u) ulog="$(realpath -m "$OPTARG")" ;;
    *) sed -n 2,6p "$0" >&2; exit 2 ;;
    esac
done
[ -s "$payload" ] || { echo "no payload: $payload (hwboot/fetch.sh)" >&2; exit 1; }
[ -e "$root/build/nxk.elf" ] || { echo "run tools/build.sh first" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$tmp"' EXIT
pid=""

OBJCOPY="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-objcopy"
"$OBJCOPY" -O binary "$root/build/nxk.elf" "$root/build/nxk.bin"
python3 "$here/mkbl31.py" -o "$tmp/bl31.bin" >/dev/null

# The card: hekate's firmware (Minerva, LP0, the L4T BPMP and SC7 blobs),
# an ini that boots [nxk] at once, and nxk's two stages.
unzip -oq "$here/dl/hekate_nyx.zip" -d "$tmp/card" 'bootloader/sys/l4t/*' 'bootloader/sys/libsys_*.bso'
cat >"$tmp/hekate_ipl.ini" <<'EOF'
[config]
autoboot=1
autoboot_list=0
bootwait=0
backlight=100
autohosoff=0
autonogc=1
updater2p=0
bootprotect=0

[nxk]
l4t=1
boot_prefixes=nxk/
uart_port=2
EOF
adds=(--add "$tmp/hekate_ipl.ini:/bootloader/hekate_ipl.ini"
    --add "$tmp/bl31.bin:/nxk/bl31.bin" --add "$root/build/nxk.bin:/nxk/bl33.bin")
while IFS= read -r f; do
    adds+=(--add "$f:/${f#"$tmp/card/"}")
done < <(find "$tmp/card" -type f)
python3 "$root/tools/mksd.py" -o "$tmp/sd.img" --size 256 --no-bin "${adds[@]}" >/dev/null

# Fuses as on a retail console: SKU 0x83 (ODIN, which Minerva requires) and
# the DRAM id in ODM4 (bits 3-7, and 12-14 for a T210B01's extended ids).
python3 - "$tmp/fuses.bin" "$dramid" <<'PY'
import struct, sys
fuses = bytearray(0x1000)  # the 0x7000F000 page; QEMU loads from 0x898
put = lambda reg, v: struct.pack_into("<I", fuses, 0x800 + reg, v)
put(0x110, 0x83)  # FUSE_SKU_INFO
i = int(sys.argv[2])
put(0x1D8, (i & 0x1F) << 3 | (i >> 5) << 12)  # FUSE_RESERVED_ODM4
open(sys.argv[1], "wb").write(fuses)
PY

args=(-object "secret,id=tegra.fuse.cache,file=$tmp/fuses.bin,format=raw"
    -machine "$machine,bootloader=$payload" -m 4G -display none
    -drive "if=sd,index=0,format=raw,file=$tmp/sd.img"
    -serial "file:$tmp/uart.log" -serial null -serial null -serial null
    -monitor "unix:$tmp/mon,server,nowait")
[ -z "$trace" ] || args+=(-d trace:memory_region_ops_write,trace:memory_region_ops_read -D "$trace")
"$qemu" "${args[@]}" >"$tmp/qemu.log" 2>&1 &
pid=$!
start=$(date +%s)
while [ $(($(date +%s) - start)) -lt "$secs" ]; do
    sleep 1
    grep -q "nxk> " "$tmp/uart.log" 2>/dev/null && break
    kill -0 "$pid" 2>/dev/null || break
done
if [ -n "$shot" ]; then
    python3 - "$tmp/mon" "$shot" <<'PY'
import socket, sys, time
m = socket.socket(socket.AF_UNIX)
m.connect(sys.argv[1])
m.sendall(f"screendump {sys.argv[2]}\n".encode())
time.sleep(2)
PY
fi
[ -z "$ulog" ] || tr -d '\r' <"$tmp/uart.log" >"$ulog"
tr -d '\r' <"$tmp/uart.log" | grep -E "nxk-bl31\] (EL3|jumping)|entered in EL|cores online|nxk> " | head -8 || true
if grep -q "nxk> " "$tmp/uart.log"; then
    echo "ORACLE OK: shell after $(($(date +%s) - start)) s ($machine, $(basename "$payload"))"
else
    echo "ORACLE FAIL: no shell after ${secs} s ($machine, $(basename "$payload"))"
    echo "-- UART-A:"
    tr -d '\r' <"$tmp/uart.log" | tail -20
    echo "-- QEMU:"
    grep -v "No secret with id" "$tmp/qemu.log" | tail -5
    exit 1
fi
