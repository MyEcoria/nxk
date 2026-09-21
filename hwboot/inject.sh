#!/usr/bin/env bash
# Sends an RCM payload to a Switch waiting in RCM (fusee-gelee).
# Usage: ./inject.sh [payload.bin]
# Default payload: hekate (boots to its menu on the console screen).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

payload="${1:-$here/dl/hekate_ctcaer_6.5.3.bin}"
[ -s "$payload" ] || { echo "[inject] missing payload: $payload (run ./fetch.sh)" >&2; exit 1; }

if [ ! -x "$here/fusee-nano/fusee-nano" ]; then
    echo "[inject] building fusee-nano..."
    make -C "$here/fusee-nano"
fi

"$here/check.sh" || {
    echo "[inject] plug the Switch in (USB data cable) and power it on first." >&2
    exit 2
}

echo "[inject] sending $payload ..."
exec "$here/fusee-nano/fusee-nano" "$payload"
