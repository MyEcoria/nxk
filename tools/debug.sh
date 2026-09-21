#!/usr/bin/env bash
# Comme run.sh, mais QEMU attend un debogueur sur le port 1234 avant de demarrer.
#   aarch64-none-elf-gdb kernel/target/.../nxk -ex 'target remote :1234'
set -euo pipefail
ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec "$ici/tools/run.sh" -s -S "$@"
