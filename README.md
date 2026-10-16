<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/nxk-logo-dark.svg">
    <img alt="nxk" src="docs/logo/nxk-logo-light.svg" width="384">
  </picture>
</p>

# nxk — AArch64 microkernel for Tegra X1

Microkernel written **entirely in Rust**, with no host OS, for the
NVIDIA Tegra X1 SoC in the Nintendo Switch (Erista and Mariko). It runs
directly on the four Cortex-A57 cores. It is a **video game console**:
the screen is a game library (`display`), the GPU draws 3D in games
(`gpu`, the engine written from its registers up), games are ordinary
Rust programs on a small SDK (`jeux/nxgfx`); and it stays a
microkernel with a shell over the USB-C console and a system dashboard.

Like Linux, it is the interface between hardware and programs, which
are written on top of system calls. But as a true microkernel, it keeps
only what requires EL1 privilege: address spaces, threads, scheduling,
IPC, capabilities, interrupt routing. **Drivers and services (files,
network…) are ordinary EL0 programs**, holding only the capabilities
entrusted to them.

Programs are written in **ordinary Rust, with the standard library**
(`std` is ported: target `aarch64-unknown-nxk`). Application software
(web server, SSH…) does not belong to the system: it is just programs
like any other.

```
EL0  Rust programs (std) · shell
     services: fs (FAT32) · net (TCP/IP) · display (library, screen, input) · input · power
     drivers : console (UART) · usbcon (USB-C) · sd · usb (xHCI) · i2c · clk · gpu
     init: process manager and name service
     ─────────── ported std · libnx (no_std) ───────────
     ─────────── svc #0: 53 system calls ───────────
EL1  nxk: memory · threads · scheduler · IPC · capabilities · IRQ · futex
```

Everything but the GPU has been verified in the `tegra_qemu` emulator. On
a **real Switch** (Erista), nxk boots through hekate on its four cores;
the `display` service shows the **game library** (cards read from
`/games`, played with touch or a gamepad, HOME to leave) and the
dashboard pages (overview, system, network, a mirror of the USB-C
terminal, screen messages); games draw with the **GPU's 3D engine**
(vertex and fragment shaders, depth, block-linear surfaces, 60 Hz
flips); the shell is reached over the USB-C console (`usbcon`: the
Switch as a USB serial device, `tools/usbterm.py` on the PC), which also
carries a debug channel (`tools/nxdbg.py`: screen capture, touch and
gamepad input), with the SD card and Wi-Fi (see "Limits" and
`hwboot/README.md`).

## Games

`jeux/` holds the game SDK (`nxgfx`, a library on `nxk-abi` alone:
screen, GPU command streams, meshes, a 2D canvas with text, input, math)
and the games. `jeux/starlane` is the sample, a 3D lane runner:

```bash
./tools/build.sh && ./tools/mksd.py -o build/sd.img --firmware firmware
# a game is /games/NAME/NAME on the card, with /games/NAME/game.txt:
#   title=Starlane  description=...  color=F0A04C
./tools/usbterm.py --put ~/.cache/nxk/target-apps/aarch64-unknown-nxk/release/starlane:/games/starlane/starlane
```

A game opens `Graphics::open()`, which powers the GPU, takes the screen
(three block-linear images the GPU draws on, flipped at the vertical
blank) and gets the input `display` routes to it; each frame is a
command stream (`Frame`: clear, pipeline, uniforms, indexed draws,
canvas) the GPU service checks and runs. The panel is portrait:
`Graphics::projection` turns the landscape picture. HOME (or the
`Event::Button` the SDK passes) ends a game; the library is back.

## Build, run, test

Prerequisites: stable Rust with `rust-src`
(`rustup component add rust-src`; targets `aarch64-unknown-none` and
`aarch64-unknown-none-softfloat`), the compiled `tegra_qemu` emulator,
`mkfs.fat` and `pyfatfs` for the SD-card image.

```bash
./tools/build.sh                  # std toolchain, programs, boot image, kernel
./tools/mksd.py -o build/sd.img   # FAT32 SD card (programs in /bin)
./tools/run.sh                    # boots; shell on the terminal (Ctrl-A X to quit)
./tools/nxtest.py -c ktest -c stdtest --sd build/sd.img   # automated tests
```

`nxtest.py` options: `--sd IMAGE` (SD card), `--net [REDIRECTIONS]`
(USB network card, e.g. `tcp::8080-:80`), `--usb-kbd` and `--keys` (USB
keyboard), `--machine tegrax1plus` (Mariko), `--monitor ELF --dtb DTB`
(boot in EL2 under a PSCI monitor), `--screendump` (screen capture),
`--usb-cmd CMD` (CMD typed on the USB-C console, the script acting as
the PC), `--usb-put LOCAL:REMOTE` (file copied to the SD card over that
console first), `--usb-net share|quiet` (with `--usb-cmd`: the PC also
turns the USB-C network on and shares its connection at 10.42.0.1, or
answers nothing), `--tap X,Y[:OUT.ppm]` (touch the screen at landscape
pixel X,Y after the commands and the `[input] touchscreen` line, then capture it), `--dbg COMMANDS` (the debug channel's `tools/nxdbg.py` commands over the USB-C console, e.g. `"shot a.png tap 640 670 button R"`). On a real Switch, `tools/usbterm.py --reload DRIVER`
replaces a running boot driver by a new build without touching the SD
card (`hwboot/README.md`, "Driver updates over USB-C").

`--usb-cmd` needs the USB device mode of the OTG controller in
`tegra_qemu`, which upstream lacks:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-usb-otg.patch`, then
rebuild it. QEMU exposes the bus on chardev `usbotg`, where
`tools/usbterm.py --emu SOCKET` plays the PC's USB host. `--tap` needs
the touch panel's interrupt line, wired to GPIO X1 as on the Switch:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-touch-irq.patch`
(after the OTG patch). `gpu on` gets past the GPU's secure boot only with
GM20B's unit counts in the emulator's GPU model:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-gpu.patch`; the GPU
firmware goes on the image with `tools/gm20b-fw.sh` then
`tools/mksd.py -o build/sd.img --firmware firmware`.

Build directories live in `~/.cache/nxk/`: the project is synced and
does not have to carry build artifacts.

## Writing a program

A program is an ordinary Rust crate in `apps/`, added to
`apps/Cargo.toml` and to the `apps` list in `tools/build.sh`:

```rust
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn main() -> std::io::Result<()> {
    let l = TcpListener::bind("0.0.0.0:8080")?;
    for s in l.incoming() {
        let mut s = s?;
        thread::spawn(move || {
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello from nxk\n");
        });
    }
    Ok(())
}
```

`tools/mkstd.py` prepares the `nxk` toolchain: a copy of stable Rust
(hard links) whose standard library receives the `std/library` port,
the `std/aarch64-unknown-nxk.json` target and its rustup declaration.
`build.sh` then compiles `apps/`; by hand:

```bash
cd apps && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=~/.cache/nxk/target-apps cargo build --release
```

(`RUSTC_BOOTSTRAP` for `-Z build-std`, see `apps/.cargo/config.toml`;
this is the build directory whose `std` `mkstd.py` invalidates when the
port changes.) Programs start from the boot image (by their name) or
from the SD card (`/bin/NAME`, or a path).

### What `std` provides

| Area | Supported | Missing |
|---|---|---|
| Memory | `Box`, `Vec`, collections; 36-size-class allocator (O(1), header-free), large blocks returned to the kernel, recycled addresses | |
| Threads | `spawn`, `scope`, names, `join`, `park`, `sleep`, `available_parallelism`, `thread_local!` with destructors; guarded stacks for later threads; `std::os::nxk::thread` (priority, core) | |
| Synchronization | `Mutex`, `RwLock`, `Condvar`, `Once`, `OnceLock`, `Barrier`, `mpsc` (kernel futex) | |
| Time | `Instant` with no system call, `SystemTime` (real-time clock) | |
| Environment | `args`, variables, current directory (`PWD`), `consts::OS = "nxk"`, `getrandom` for `HashMap` | `current_exe` approximated |
| Files | FAT32: read, write, append, `seek`, directories, `rename`, `copy`, `remove_dir_all`, `metadata` (size, date), `try_clone` | links, permissions, `set_times`, locks |
| Network | IPv4: `TcpStream`, `TcpListener`, `UdpSocket`, timeouts, non-blocking mode, `try_clone`, DNS resolution | IPv6, `peek`, multicast, loopback |
| Processes | `Command`: arguments, environment, directory; `spawn`, `status`, `output`, `wait`, `try_wait`, `kill`, `id`; pipes (`piped`, `io::pipe`, `a \| b` with no relay), `null` | redirection to a file |
| Standard I/O | console or pipes, `IsTerminal` | |
| Panics | message on standard error, abort (`panic=abort`) | unwinding, backtraces |

## Layout

```
abi/        nxk-abi: syscall numbers, errors, rights, service protocols
            (shared by the kernel, libnx and std: single source of truth)
kernel/     the microkernel
user/       libnx (no_std system library), init, shell, drivers and services
std/        Rust standard library port, linker script, target
apps/       ordinary Rust (std) programs: hello-std, stdtest
jeux/       games (std) and nxgfx, their SDK: Starlane, a cube
bench/      comparison against L4T: EL3 monitor, Linux measurement program, scripts
tools/      build, SD card, std toolchain, launch, automated tests,
            usbterm.py (USB-C console terminal on the PC),
            nxdbg.py (screen capture, touch and gamepad over USB-C)
hwboot/     physical Switch bring-up: RCM injector, hekate entry, nxk-bl31 EL3 monitor
refs/       hekate, Atmosphere: hardware documentation (never compiled)
docs/logo/  pixel-art logo (regenerate with tools/mklogo.py)
```

About 8,000 lines of Rust for the kernel, 7,000 for drivers, services
and libnx, 3,800 for the `std` port.

## System calls

Linux arm64 conventions: `svc #0`, number in `x8`, arguments in
`x0`–`x5`, result in `x0`, errors as `-errno` (Linux values). Blocking
calls take a timeout (ns; 0 = unbounded).

| Family | Calls |
|---|---|
| Base | `nop`, `yield`, `sleep`, `clock` (monotonic, real-time), `clock_set`, `getcpu`, `getpid`, `gettid`, `getrandom`, `debug_write`, `thread_exit`, `process_exit`, `system_power` |
| Handles | `handle_close`, `handle_dup` (reduced rights, badge), `handle_revoke` |
| Memory | `mem_map` (on demand or populated, free or fixed address, W^X), `mem_unmap`, `mem_protect`, `shm_create`/`shm_map`/`shm_paddr` (contiguous, uncached, below 4 GiB for DMA), `mmio_map` (read-only without the `WRITE` right), `mmio_derive` |
| Threads | `thread_create`, `thread_set` (priority, core), `futex_wait`, `futex_wake` |
| Processes | `process_spawn` (static-PIE ELF, arguments, environment), `process_wait`, `process_kill` |
| IPC | `endpoint_create` (pipe option), `ipc_call`, `ipc_send`, `ipc_recv`, `ipc_reply`, `ipc_reply_recv`, `ipc_reply_to` |
| Notifications, IRQ | `notify_create`, `notify_signal`, `notify_wait`, `notify_bind`, `irq_get`, `irq_bind`, `irq_ack` |
| Inspection, debugging | `sysinfo` (processes, memory, cores with their timer-interrupt lateness and wake-ups for nothing), `klog_read`, `klog_bind` (kernel log, console only), `debug_attach`, `debug_regs`, `debug_mem`, `debug_threads`, `debug_stop` |

Time is read with no system call: the kernel opens `CNTVCT_EL0` to EL0
(like the Linux vDSO).

## Kernel design

**One kernel stack per core, not per thread** (seL4 model). Registers
are stored directly in the thread control block; the kernel never blocks
in the middle of a call, and never waits on a device: its messages go
to an 8 KiB log ring that the `console` service drains (`klog_bind`,
`klog_read`); only the boot log, before that service starts, and panics
are printed directly.

**One global lock**, as in multiprocessor seL4: kernel paths last a few
microseconds. Calls with no shared state (`clock`, `getcpu`, `gettid`)
take a lock-free fast path. Long work (zeroing a large shared-memory
object or populated mapping, unmapping megabytes) lets the lock go every
64 pages, so other cores wait at most one such batch.

**Capabilities**: every object (endpoint, thread, process,
notification, shared memory, interrupt line, register range) is named by
a handle carrying rights and a generation; revocation by generation
change. Growable object tables.

**Synchronous IPC**: 6 words in registers, up to 4 KiB via each
thread's control page (UTCB), up to 4 handles transferred; the kernel
attaches the sender's badge and pid to every message. Client and server
on the same core: direct switch with no scheduler. Timeouts on all
blocking calls. A "pipe" endpoint signals end of stream when its last
writer disappears.

**Tickless scheduler**: 32 priorities, O(1) selection, one queue per
core; the timer is programmed for the next deadline (wakeup, or end of
quantum only if a thread of the same priority is waiting). Balancing
toward idle cores, migration, adjustable affinity and priority. A core
with no work gets no periodic interrupt: `ktest` counts the interrupts
that leave a core idle (`sysinfo`), not the ones services cause by
polling (statistics each second, the fan's temperature every 2 s).

**Memory**: buddy allocator (4 KiB–4 MiB); pages zeroed with `DC ZVA`;
ASID-tagged address spaces; on-first-access anonymous pages and stacks
(including multi-core races); enforced W^X; ASLR (static-PIE executables
relocated by the loader, heap, stacks, registers); reserved areas
(memory-controller carveouts, `/reserved-memory` from the device tree)
excluded.

**Security**: Spectre v2 mitigation (predictor invalidated on process
switch), W^X, ASLR, fault isolation (a faulty program is terminated,
the rest keeps running).

**Debugging**: a debugger (endpoint) receives a process's faults,
breakpoints and single-steps, reads and writes registers and memory,
stops threads.

**Boot**: directly from reset (EL3), or in EL2 under a PSCI monitor
(as on a Switch with its secure firmware); DRAM size detected; device
tree read when provided. On a Switch, hekate's L4T entry loads
`hwboot/nxk-bl31`, an EL3 monitor doing ATF's job, which enters nxk in
EL2 and starts the other cores through PSCI `CPU_ON`. If the loader left the screen on
(hekate), the kernel log is copied there until the `display` service
takes over: the only visible output without a UART adapter.

## Services and drivers

All in EL0, started by `init` with only their capabilities.

| Service | Role | State |
|---|---|---|
| `console` | UART-A, line editing; mirrored to a terminal driver (input and all output). Output never waits: one 16 KiB ring, drained to UART-A by its TX-empty interrupt and read by the terminal driver at its own pace; the kernel log is drained into it. The screen reads the same ring without taking from it (`TAIL`, screen badge). Only the shell's handle (session badge, inherited by the programs it starts) writes the session: every other write and the kernel log come between `LOG_START`/`LOG_END` marks (private OSC 7700), so readers tell logs from commands and their results | emulator; on a Switch, UART-A pads are internal (not on the Joy-Con rail) |
| `usbcon` | USB-C port as a composite USB device (1209:0001) on the Erista OTG controller: a serial console (CDC-ACM), the terminal of a PC (`tools/usbterm.py`), and a network function (CDC-ECM) published as link `usb0`, over which the PC may share its connection (`hwboot/README.md`, "Internet over USB-C"). One thread: console output is pulled into the DMA buffer when the IN endpoint is free, 100 ms after the PC opens the terminal (its tty echoes until it is raw); frames queued each way, the carrier given by the host's choice of the data interface's alternate setting; traffic counters and state for the screen (`STATS`); a debug channel inside the serial stream (APC frames `ESC _ N base64 ESC \\`, taken out of the shell's input): screen capture, touch and gamepad injection for `tools/nxdbg.py` (`hwboot/README.md`, "Debug channel") | emulator (virtual host: console, network with DHCP and ping via `nxtest.py --usb-net`); **Erista**: the way in to the shell; network to validate; Mariko has no such controller (stops) |
| `clk` | clocks and resets (CAR), USB2 PLLs, power-partition state (PMC); the PWM block is never reset nor stopped once running (channel 0 dims the Erista backlight) | emulator |
| `i2c` | six I2C buses; a write and a read joined by a repeated start on request (`REPEATED_START`, packet mode) | emulator |
| `power` | power off and reboot (MAX77620 PMIC), real-time clock, temperatures (TMP451, started again as hekate's `tmp451_init`: Nyx shuts it down, freezing its last reading; Mariko's -12.5 C offset), SD, PCIe and touchscreen rails (LDO2, LDO1, LDO6), the charger's input status (BQ24193 power good: battery or external power) | emulator; **Erista**: charger read on USB power |
| `fan` | the fan as hekate's `bdk/thermal/fan.c` drives it: PWM channel 1 (inverted duty, 236 steps), pad V4, the battery's 5V regulator (GPIO A5, also the Joy-Con rails' source: a Joy-Con driver will have to share it; Erista's USB VBUS source, CC4, kept off). Every 2 s from the SoC temperature (`power`, TMP451 remote): Nyx's steps (off up to 39 C, 22% from 42 C, 32% from 47 C, 43% from 52 C), full speed from 60 C until below 57 C; `SET` puts a duty by hand, still overridden when hot; a sensor lost for 3 readings gives at least 43%. Speed from the tachometer (GPIO S7, two pulses a turn) counted on the GPIO bank 4 interrupt (SPI 55) for 1 s: on `MEASURE`, 3 s after each change, then every minute. Nyx stops the fan before chaining to nxk | emulator (no PWM output nor tachometer there: 0 rpm); **Erista**: to validate |
| `sd` | SD card (SDHCI, ADMA2 straight into `fs`'s uncached, contiguous buffer: no bounce copy), slot power cycle (239 ms off, as hekate) then power-up (pads, GPIOs, PMC I/O power, PMIC LDO2): a card hekate left in 1.8 V signaling starts over at 3.3 V. Block 0 is read before publishing; a card that answers commands but sends no data is reported (host, pads, rails, CMD13 state) and retried: first on a 1-bit bus in place (ACMD6, card kept powered: a slot that carries DAT0 alone, a loose connector, `hwboot/README.md` runs 19 and 20), then after a 2 s power cycle, on a 1-bit bus after a 239 ms one, last at 1.8 V; the ready line gives the bus width; **Erista**: 64 GB card read and written | 3.3 V default speed; the 1-bit fallback (about 3 MB/s at 25 MHz) not yet run on hardware |
| `fs` | FAT32 (long names), write-back cache with readahead | emulator |
| `cpufreq` | the CPU clock at sys-clk's steps (612 to 1785 MHz) on PLLX, parked on PLLP while PLLX relocks (the PLLP_OUT_CPU branch opened first, CCLKG switched, PLLX turned off only once a timed loop shows the CPU slowed down); the CPU rail (MAX77621, I2C5) at the voltage NVIDIA's L4T gives a PLLX-sourced CPU (`tegra210-dvfs.c`, CPU_PLL_CVB_TABLE_ODN: the chip's CPU speedo from the fuses, +30% margin, 950 to 1225 mV), raised before a faster clock and lowered after a slower one. Steps above 1225 mV are refused (ERANGE): about 1122 to 1224 MHz on an Erista, the rest needs the DFLL (not written). Every boot keeps the loader's clock (hekate: 998.4 MHz, 950 mV), and the service goes back to it when the SoC reaches 80 C. Erista Switch (SKU 0x83) only; the pure part (`dvfs.rs`) has host tests | emulator (sequence checked with fake fuses, not committed; no fuses there: boot clock only); **Erista**: to validate |
| `display` | screen (window A) as a touch dashboard (`docs/ui/home.html` is its mockup): a header (logo, USB, Ethernet, Wi-Fi, SD and fan states, temperature, uptime), a tab bar and five pages. HOME: CPU, memory, SD card free space, network, the last commands; SYSTEM: per-core load and the GPU's (share of time with a batch in flight), the CPU and GPU clocks and rails, syscall/interrupt/switch rates, memory, temperatures, the fan (mode, duty, last speed), reboot and power off (confirmed); the FAN panel (header chip or the fan row): AUTO or MANUAL, the duty by 10% steps (`fan::SET`, sent from the worker thread); the CPU panel (the cpu clock row): sys-clk's steps, those over the rail's maximum greyed out, and BOOT (`cpufreq::SET`, from the worker thread); the GPU panel (the gpu clock row): sys-clk's steps, those over the rail's maximum greyed out, those past the power source's cap in the warning color and set with `CLOCK_FORCE` once confirmed (`gpu::CLOCK`, from the worker thread); the GPU's state, load and clock settings come from its stats page, current while it runs a batch; NETWORK: address, gateway and DNS of the link in use, the wired link, Wi-Fi with scan, recovery (`wifi::RETRY`) and leave; TERMINAL: the USB-C console as a zsh-like terminal, read from the console ring (prompts, directory and failed commands from the shell's OSC 7/133 marks, ANSI colors, `wifi connect` passphrase masked), the shell's session only; LOGS: the logs (the console's marked writes: services, init, the kernel; and `fbecho`). Statistics every second, each service asked with a 20 ms cap; slow Wi-Fi requests on a worker thread. A frame is drawn whole into a cached image, then only changed pixels are copied to the displayed one (Terminus font; a portrait panel is drawn turned); a gamepad (`input` button events) moves an outline over the tappable zones (D-pad), taps the outlined one (A), backs out (B) and turns the page (L, R); `SNAPSHOT` hands out the displayed image read-only for captures | emulator (touch via `--tap`); **Erista**: takes over the 720x1280 panel hekate left on; touch to validate; own panel init missing |
| `input` | touchscreen (FTM3BD56) powered up and started as hekate does (reset pin J7, LDO6 through `power`, system reset, finger mode; Nyx leaves it off), read with repeated starts when its interrupt line says it has events (GPIO X1, level low, as in the Switch's Linux device tree; bank 6 interrupt, SPI 87), polled if the line is missing or stuck; each reader asks for the event kinds it wants (`READ` word 1); USB keyboard; button events (gamepad, `BTN_*`) injected by other drivers: `usbcon`'s debug channel is the only source for now, no Joy-Con driver | emulator (with the touch patch); **Erista**: power-up and interrupt pin to confirm (hekate polls) |
| `usb` | xHCI: HID keyboard, CDC-ECM network card | emulator; the real xHCI needs an NVIDIA firmware; stops if `clk` does not report it powered |
| `net` | TCP/IP (smoltcp) over one link at a time: `usb0` (the PC's shared connection over USB-C), `eth0` (USB adapter), then `wlan0`, the first with a carrier, checked every 2 s; a wired link without a DHCP address for 10 s gives way until its carrier drops; a switch changes the MAC, starts DHCP over and aborts TCP connections. DHCP (retried every 2 s, started over when a link drops), DNS, TCP, UDP, ICMP echo (`nettest ping HOST`), traffic counters and the link in use (`STATS`); **Erista**: over Wi-Fi, `nettest ping google.com` answered | |
| `wlan` | Wi-Fi station: Tegra PCIe root complex (rail, UPHY lanes, AFI windows, link, BAR assignment) and BCM4356 identification, firmware boot (TCM download, NVRAM, shared area), msgbuf control and data rings (RX buffers, a TX flow ring) driven by the chip's interrupt (PCIe2 mailbox, legacy INTx), scans, joins of open and WPA2-Personal networks (CCMP, our own supplicant: 4-way and group key handshakes, keys handed to the firmware); publishes the link as `wlan0` once associated (`net` takes it when there is no `eth0`: DHCP, DNS, TCP) and its control as `wifi` (`wifi status` names the step where bring-up stopped, `wifi retry` runs it again or resets a running chip; `wifi connect` returns once `net` has its DHCP address) | the chip still resets itself at times, joined or idle (hardware runs 16, 20, 21, cause unknown): `wlan` checks it every second, reports the loss in `wifi status` and brings it back by itself, rejoining the last network. WPA (TKIP), WPA/WPA2 mixed, WPA3-only and networks requiring management frame protection are refused with `EOPNOTSUPP`. Without a PCIe device (emulator) a simulated adapter answers, so `wifi` and the `net` attach are still exercised |

The `fs` and `net` services attach each open file or socket to the
process that opened it: another process cannot use it, and the ones of a
program that vanished without closing them (crash) are reclaimed.

## Features

| Area | Done | Remaining |
|---|---|---|
| Boot | 4 cores, EL3 or EL2 + PSCI, Erista and Mariko, DRAM detection; real Erista via hekate + nxk-bl31 (4 cores, shell over USB-C) | Mariko hardware |
| Memory | on demand, W^X, `mprotect`, shared memory, DMA buffers, ASLR, reserved areas | |
| Threads | priorities, affinity, migration, balancing, tickless, futex | deep sleep states |
| IPC | synchronous, timeouts, handles, notifications, pipes, sender identity | wait on several objects (`poll`) |
| Programs | static-PIE ELF, arguments, environment, TLS, load from SD card | dynamic linking (ruled out) |
| Rust | `no_std` (libnx) and **`std`** (see "What `std` provides") | panic unwinding, backtraces |
| Storage | SD, FAT32, file API | eMMC, ext4 |
| Network | USB-Ethernet, Wi-Fi (BCM4356: scans, open and WPA2 joins, traffic on `wlan0`), TCP/IP, sockets | IPv6 |
| Hardware | UART, I2C, PMIC, RTC, temperatures, fan, CPU clock on PLLX, screen, touchscreen, USB host, USB device (serial console, Ethernet), GPU (secure boot, 2D engine, compute kernels, 3D engine with shaders and depth, sys-clk's clock steps 76.8 to 921.6 MHz) | GPU textures, audio, Joy-Con, buttons (GPIO), CPU DFLL and other clocks (DVFS), battery |
| Security | capabilities, revocation, Spectre v2, isolation | |
| Tools | build, SD card, tests (`ktest` 29, `stdtest` 17, `wlan` crypto and WPA2 on the PC), debugger (gdb), L4T comparison, USB-C terminal | |

## Limits

- **On a real Switch** (Erista, hekate L4T entry) nxk reaches the
  shell over the USB-C console (`usbcon`, `tools/usbterm.py`): UART-A
  pads are internal. Last hardware run: single core (before PSCI in
  nxk-bl31) and no SD card (see `hwboot/README.md`, "Hardware runs").
  A USB keyboard does not help: its keys go to `input`, not to the
  shell, and hekate's L4T path leaves the xHCI unpowered. Seen there:
  4 cores, SD card and filesystem, `ktest` 29/29, `stdtest` 17/17
  (`hwboot/README.md`, "Hardware runs").
- Kernel messages (`[nxk]` fault reports) reach the USB console through
  `console`, boot log included; before `console` starts and on a panic
  they go to UART-A and the loader's screen. Mariko lacks the USB2 OTG
  device controller (its OTG port belongs to XUSB): no USB console
  there.
- The real USB controller (xHCI) needs a proprietary NVIDIA firmware;
  the SD reader needs power and pin setup; without hekate the screen
  needs DSI panel init; buttons need GPIOs the emulator cannot drive.
- Wi-Fi: `wlan` powers the module (PMIC LDO1,
  WIFI_EN on PH0, WIFI_RST on PH1), sets the PEX and module pads as
  Horizon does, brings the PCIe root complex up, then should train the
  link, read the BCM4356 chip identity and boot the console's own
  firmware in it: on an Erista, `wifi status` reports `BCM4356 rev 3 on
  PCIe port 1, firmware up` with the firmware's console (`hwboot/README.md`,
  runs 5 to 14). Commands, scans (`wifi scan`) and joins go through
  Broadcom's msgbuf rings. The console's firmware has no supplicant
  (Horizon runs its own), so `wlan` has one for WPA2-Personal (`wpa.rs`,
  with SHA-1, PBKDF2, the 802.11 PRF and AES key unwrap in `crypto.rs`):
  this firmware reports received EAPOL frames as `E_EAPOL_MSG` events,
  the replies leave on the TX flow ring, and the keys go back with
  `wsec_key`. Their known-answer and handshake tests run on the PC:
  `rustc --edition 2021 --test user/wlan/hosttest.rs -o T && ./T`.
  Data frames use the RX and TX rings, drained on the chip's interrupt
  (a 500 ms poll stays as a safety net). The chip still resets itself
  at times, joined or idle, with no trap or assert from its firmware
  (hardware runs 16, 20, 21): `wlan` checks it every second (crash
  flags, a bus reading all ones, memory decoding off), reports the loss
  (`wifi status`: link, config space, module pins, the firmware's last
  console lines) and brings it back by itself (module reset under
  PERST#, link, firmware, then the last network with the passphrase it
  keeps), retrying after 2 s, then up to every minute; `wifi retry`
  does the same by hand. Its firmware
  and calibration data are proprietary and never in the repository:
  they are extracted from the console's own
  system titles with `hwboot/nxfw-extract.py` into `firmware/` (git
  ignores it), then copied to the card by `tools/mksd.py --firmware DIR`
  or `hwboot/mksd-hw.sh MOUNT DIR`, which put them in `/firmware/brcm`.
  The whole procedure is in `hwboot/README.md` ("Wi-Fi firmware"). The
  station MAC is a locally administered one: the real address lives in
  CAL0 on the eMMC, which nxk cannot read. None of the PCIe path runs
  in the emulator, which has no root complex to model: there a
  simulated adapter answers `wifi` instead.
- GPU (GM20B): `gpu` powers it only when asked (`gpu on`, or a 2D or
  compute request): its rail (the MAX77621 at 0x1C, NVIDIA's voltage
  for 76.8 MHz plus 25 mV), clocks and clamps through `clk`, the GPCPLL
  in noise-aware mode at 76.8 MHz, then NVIDIA's secure boot:
  the ACR loader from linux-firmware runs on the PMU, copies FECS into
  the GPU firmware carveout hekate's L4T entry locks, and starts it;
  GPCCS, the golden context and one channel follow nouveau's GM20B code.
  It runs 2D fills and copies on surfaces clients attach, three
  hand-assembled SM 5.3 kernels (fill, copy, SAXPY) and an FFMA
  benchmark (`gpu test`, `gpu bench`). `gpu clock` moves it among
  sys-clk's steps (76.8 to 921.6 MHz, 76.8 MHz apart) while it runs, as
  nouveau's `gm20b_clk_prog` does: the rail at NVIDIA's voltage for the
  step (`gm20b_na_cvb_coef`, the GPU speedo, plus 25 mV) raised first
  and lowered last, a safe PLL while the noise-aware detector moves,
  then an NDIV slide. Each change is measured (SM cycles against the
  CPU's clock; more than 5% off goes back to 76.8 MHz). sys-clk's caps
  apply from `power`'s charger status: 460.8 MHz on battery, 768 MHz on
  input power; 844.8 and 921.6 MHz need `force` (the official charger
  and the dock are not told from a PC port yet). Above 76.8 MHz the SoC
  reaching 80 C sends it back there. `gpu sweep` benchmarks each step,
  `bench full` loads the CPU and the GPU together. A stats page
  (`gpu::STATS`, shared read-only, changed under a sequence count)
  carries its state, clock settings, the steps this chip runs and its
  busy time (batches from doorbell to fence, the one in flight
  included), so the screen and `gpu load` follow it without waiting on a
  service busy with a batch; the screen's GPU panel overclocks past the
  cap once confirmed. Its firmware is NVIDIA's
  redistributable set (`tools/gm20b-fw.sh` copies it from the PC's
  `/lib/firmware`), kept out of the repository like the Wi-Fi blobs.
  Runs on an Erista Switch (`gpu test` 5/5 at every step, `gpu bench`
  30.4 GFLOP/s at 76.8 MHz to 365.8 at 921.6; `hwboot/README.md`, "GPU"
  and runs 26 and 27); Erista only (Mariko's GPU rail is on a
  MAX77812); no 3D.
  The emulator's GPU model runs the whole power-up with
  `tools/tegra_qemu-gpu.patch` but renders and computes nothing:
  `gpu test` fails there by design.
- Audio, Joy-Con and frequency scaling are not done (the last one
  risks damaging hardware).
- A pipe has no buffer: the writer waits until a reader takes its data.
  `output()` reads both outputs in parallel for this reason.
- The `fs` service refuses to delete a file that is still open
  (`EBUSY`), like Windows rather than Unix.

## Hardware traps

1. The GIC starts with all its lines in Group 0 (secure world): an EL1
   non-secure kernel receives nothing until they are switched to Group 1
   from EL3 — the role of the secure monitor on a real system.
2. Waking a secondary core requires, in order: reset vector in the SB
   block, power partition at the PMC, voltage-lock removal, CAR reset
   lines.
3. Several registers have an unknown reset value on silicon but a sane
   one under QEMU: CNTFRQ_EL0 and CPUECTLR_EL1.SMPEN (to set in EL3
   before caches and MMU), SCTLR_EL2 and MDCR_EL2 (the latter can trap
   EL1 debug-register accesses to EL2).
4. With the MMU off, memory is Device type: exclusive accesses of
   atomics (LDXR/STXR, no LSE on A57) are not guaranteed there. No lock
   before the MMU is enabled.
5. hekate (L4T entry) places memory-controller carveouts below TZ-DRAM
   (running BPMP firmware, GPU areas forbidden to the CPU): they are
   excluded from free memory like TZ-DRAM.
6. Reading a block with no clock, in reset, or whose PMC partition is
   off hangs the bus: the whole system stops when it has a single core.
   The emulator answers anyway. A partition-gated block driver (XUSB)
   therefore asks `clk` for `ENABLE` then `STATUS`, which never powers a
   partition on by itself, and only touches its registers once
   `ST_READY` is complete. Fuses too: `usbcon` reads its PHY trims only
   when `clk` reports the fuse block clocked and visible.
7. hekate turns the AHB memory prefetcher on for USB when it serves UMS
   and flushes it before every transfer; left on, a device re-reading a
   buffer the CPU rewrote can get stale data. The kernel turns it off at
   boot, so uncached `shm` buffers stay coherent for DMA.

## L4T comparison

See `bench/README.md` and `bench/RESULTS.md`. In the emulator, creating
then waiting for a thread costs about 300 µs with `std` on nxk (226 µs
with libnx), versus 836 µs with pthread on L4T.

## Licenses

The screen font comes from Terminus (Dimitar Zhekov, SIL Open Font
License 1.1), converted at build time from the host system console font.
