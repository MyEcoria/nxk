<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/nxk-logo-dark.svg">
    <img alt="nxk" src="docs/logo/nxk-logo-light.svg" width="384">
  </picture>
</p>

# nxk — AArch64 microkernel for Tegra X1

nxk turns a Nintendo Switch (Tegra X1, Erista and Mariko) into its own
video game console. It is a microkernel written **entirely in Rust**,
with no host OS: the kernel keeps only memory, threads, scheduling, IPC
and capabilities, and everything else (drivers, files, network, the
screen) runs as ordinary programs. The screen is a game library with
users, settings and system pages; games are ordinary Rust programs
drawing with the GPU's 3D engine through a small SDK; cloud games
stream from Amazon Luna. It boots on a real Erista through hekate.

Building, running, testing and the design: [DEV.md](DEV.md).

## Features

| Area | Done | Remaining |
|---|---|---|
| Console | game library (touch and gamepad, HOME), users with avatars and PIN, dark and light themes, quick settings, system, network and terminal pages, sleep with the GPU rail off | |
| Games | SDK `nxgfx` (GPU command streams, meshes, 2D canvas, input, audio), 3D engine with shaders and depth, ray queries in compute shaders; games Cube, Lumen, Starlane | |
| Cloud games | Amazon Luna as a catalog: sign-in by code, the account's games, H.264 video and Opus audio stream, performance overlay | 60 Hz stream (45 Hz by default: the decoder misses frames at 60) |
| Boot | 4 cores, EL3 or EL2 + PSCI, Erista and Mariko, DRAM detection; real Erista via hekate + nxk-bl31 (4 cores, shell over USB-C) | Mariko hardware |
| Memory | on demand, W^X, `mprotect`, shared memory, DMA buffers, ASLR, reserved areas | |
| Threads | priorities, affinity, migration, balancing, tickless, futex | deep sleep states |
| IPC | synchronous, timeouts, handles, notifications, pipes, sender identity | wait on several objects (`poll`) |
| Programs | static-PIE ELF, arguments, environment, TLS, load from SD card | dynamic linking (ruled out) |
| Rust | `no_std` (libnx) and **`std`** (see [What `std` provides](DEV.md#what-std-provides)) | panic unwinding, backtraces |
| Storage | SD, FAT32, file API | eMMC, ext4 |
| Network | USB-Ethernet, Wi-Fi (BCM4356: scans, open and WPA2 joins, traffic on `wlan0`), TCP/IP, sockets | IPv6 |
| Bluetooth | BCM4356 on UART-D (patch RAM, 3 Mbaud), pairing (SSP), HID host for Joy-Cons and Pro Controllers, reconnection on a button press (a Pro Controller on an Erista) | Joy-Cons on hardware, rumble, motion sensors, players, Bluetooth audio, BLE |
| Hardware | UART, I2C, PMIC, RTC, temperatures, fan, CPU clock on PLLX, screen, touchscreen, USB host, USB device (serial console, Ethernet), speakers (codec, audio hub), GPU (secure boot, 2D engine, compute kernels, 3D engine with shaders, depth and textures, sys-clk's clock steps 76.8 to 921.6 MHz) | GPU mipmaps and float render targets (written, not yet run on the console), audio quality in games, the right Joy-Con (not detected on the Erista tried), Joy-Con charging, rumble, the Power and volume buttons (read, not yet checked by hand), CPU DFLL and other clocks (DVFS), battery |
| Security | capabilities, revocation, Spectre v2, isolation | |
| Tools | build, SD card, tests (`ktest` 31, `stdtest` 19, `wlan` crypto and WPA2 on the PC, an emulated Bluetooth controller), debugger (gdb), L4T comparison, USB-C terminal | |

## Screenshots

Taken in the `tegra_qemu` emulator (`tools/nxtest.py --dbg "... shot"`).

| | |
|---|---|
| ![Game library](docs/screenshots/library.png) | ![Game library, light theme](docs/screenshots/library-light.png) |
| Game library | Light theme |
| ![Quick settings](docs/screenshots/quick-settings.png) | ![First start](docs/screenshots/welcome.png) |
| Quick settings (`+`) | First start |
| ![System](docs/screenshots/system.png) | ![Network](docs/screenshots/network.png) |
| System | Network |
| ![Terminal](docs/screenshots/terminal.png) | ![System Settings](docs/screenshots/settings.png) |
| USB-C terminal mirror | System Settings |

## License

MIT OR Apache-2.0, at your option, except a few parts derived from
other projects (hekate's EMC and display code under GPL-2.0, libopus,
OpenH264, fusee-nano): see [LICENSE](LICENSE).

nxk is not affiliated with, endorsed by or sponsored by Nintendo, NVIDIA
or Amazon. Nintendo Switch and Joy-Con are trademarks of Nintendo; Tegra
is a trademark of NVIDIA; Amazon Luna is a trademark of Amazon.com, Inc.
or its affiliates.
