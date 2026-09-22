#!/usr/bin/env bash
# Like run.sh, but QEMU waits for a debugger on port 1234 before starting.
#   aarch64-none-elf-gdb kernel/target/.../nxk -ex 'target remote :1234'
set -euo pipefail
ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec "$ici/tools/run.sh" -s -S "$@"
