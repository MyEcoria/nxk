#!/usr/bin/env bash
# Detects a Switch waiting in RCM (NVIDIA APX 0955:7321).
# Exit 0 if present, 2 if absent. No writes, safe to run anytime.
set -u
if lsusb -d 0955:7321 2>/dev/null | grep -q "0955:7321"; then
    echo "[check] Switch in RCM detected:"
    lsusb -d 0955:7321
    exit 0
else
    echo "[check] no RCM device (0955:7321) found." >&2
    echo "[check] With AutoRCM: power the console ON while USB is plugged in;" >&2
    echo "[check] screen stays black, it should enumerate as NVIDIA APX." >&2
    exit 2
fi
