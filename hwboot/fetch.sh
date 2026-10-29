#!/usr/bin/env bash
# Downloads the RCM payload + SD bootloader files (pinned versions).
# Output goes to hwboot/dl/ (git-ignored). Re-run anytime to refresh.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dl="$here/dl"
mkdir -p "$dl"

HEKATE_VER="6.5.4"
NYX_VER="1.9.4"
base="https://github.com/CTCaer/hekate/releases/download/v${HEKATE_VER}"

get() {
    local url="$1" out="$2"
    if [ -s "$out" ]; then
        echo "== present: $out"
        return
    fi
    echo "== fetching $url"
    curl -sSL --max-time 120 -o "$out" "$url"
}

get "$base/hekate_ctcaer_${HEKATE_VER}.bin" "$dl/hekate_ctcaer_${HEKATE_VER}.bin"
get "$base/hekate_ctcaer_${HEKATE_VER}_Nyx_${NYX_VER}.zip" "$dl/hekate_nyx_${NYX_VER}.zip"
# mksd-hw.sh takes the pinned pack under a fixed name.
ln -sf "hekate_nyx_${NYX_VER}.zip" "$dl/hekate_nyx.zip"

ls -lh "$dl"
# Sanity: hekate RCM payload must fit the ~196 KiB IRAM window (fusee-gelee).
size=$(stat -c%s "$dl/hekate_ctcaer_${HEKATE_VER}.bin")
[ "$size" -lt 200704 ] || { echo "payload suspiciously large: $size" >&2; exit 1; }
echo "OK: hekate payload ${size} bytes (< 196 KiB IRAM limit)"
