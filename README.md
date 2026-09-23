<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/nxk-logo-dark.svg">
    <img alt="nxk" src="docs/logo/nxk-logo-light.svg" width="384">
  </picture>
</p>

# nxk — AArch64 microkernel for Tegra X1

Microkernel written **entirely in Rust**, with no host OS, for the
NVIDIA Tegra X1 SoC in the Nintendo Switch (Erista and Mariko). It runs
directly on the four Cortex-A57 cores, server-oriented: no graphical
interface, a shell over the serial console.

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
     services: fs (FAT32) · net (TCP/IP) · display · input · power
     drivers : console (UART) · usbcon (USB-C) · sd · usb (xHCI) · i2c · clk
     init: process manager and name service
     ─────────── ported std · libnx (no_std) ───────────
     ─────────── svc #0: 51 system calls ───────────
EL1  nxk: memory · threads · scheduler · IPC · capabilities · IRQ · futex
```

Everything has been verified in the `tegra_qemu` emulator. On a **real
Switch** (Erista), nxk boots through hekate up to user space on one
core: the `display` service takes over the screen; the shell has not
been reached there yet. The USB-C console (`usbcon`: the Switch as a
USB serial device, `tools/usbterm.py` on the PC) is the way in, tested
in the emulator only (see "Limits" and `hwboot/README.md`).

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
console first). On a real Switch, `tools/usbterm.py --reload DRIVER`
replaces a running boot driver by a new build without touching the SD
card (`hwboot/README.md`, "Driver updates over USB-C").

`--usb-cmd` needs the USB device mode of the OTG controller in
`tegra_qemu`, which upstream lacks:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-usb-otg.patch`, then
rebuild it. QEMU exposes the bus on chardev `usbotg`, where
`tools/usbterm.py --emu SOCKET` plays the PC's USB host.

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
bench/      comparison against L4T: EL3 monitor, Linux measurement program, scripts
tools/      build, SD card, std toolchain, launch, automated tests,
            usbterm.py (USB-C console terminal on the PC)
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
| Inspection, debugging | `sysinfo` (processes, memory, cores), `debug_attach`, `debug_regs`, `debug_mem`, `debug_threads`, `debug_stop` |

Time is read with no system call: the kernel opens `CNTVCT_EL0` to EL0
(like the Linux vDSO).

## Kernel design

**One kernel stack per core, not per thread** (seL4 model). Registers
are stored directly in the thread control block; the kernel never blocks
in the middle of a call.

**One global lock**, as in multiprocessor seL4: kernel paths last a few
microseconds. Calls with no shared state (`clock`, `getcpu`, `gettid`)
take a lock-free fast path.

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
with no work gets no periodic interrupt (services do not wake up on
their own either).

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
| `console` | UART-A, line editing; mirrored to a terminal driver (input and all output) | emulator; on a Switch, UART-A pads are internal (not on the Joy-Con rail) |
| `usbcon` | USB-C port as a USB serial device (CDC-ACM, 1209:0001) on the Erista OTG controller: the terminal of a PC (`tools/usbterm.py`) | emulator (virtual host); **Erista**: not tried yet; Mariko has no such controller (stops) |
| `clk` | clocks and resets (CAR), USB2 PLLs, power-partition state (PMC) | emulator |
| `i2c` | six I2C buses | emulator |
| `power` | power off and reboot (MAX77620 PMIC), real-time clock, temperatures (TMP451) | emulator |
| `sd` | SD card (SDHCI, ADMA2), slot power-up (pads, GPIOs, PMC I/O power, PMIC LDO2); **Erista**: 64 GB card read and written | 3.3 V default speed only (no UHS / 1.8 V) |
| `fs` | FAT32 (long names), write-back cache with readahead | emulator |
| `display` | screen (window A), boot logo and landscape text terminal (Terminus font; a portrait panel is drawn turned) | emulator; **Erista**: takes over the 720x1280 panel hekate left on; own panel init missing |
| `input` | touchscreen (FTM3BD56), USB keyboard | emulator; polled only when someone reads |
| `usb` | xHCI: HID keyboard, CDC-ECM network card | emulator; the real xHCI needs an NVIDIA firmware; stops if `clk` does not report it powered |
| `net` | TCP/IP (smoltcp): DHCP, DNS, TCP, UDP | emulator |
| `wlan` | Wi-Fi station: Tegra PCIe root complex (rail, UPHY lanes, AFI windows, link, BAR assignment) and BCM4356 identification, firmware boot (TCM download, NVRAM, shared area), msgbuf control and data rings (RX buffers, a TX flow ring) driven by the chip's interrupt (PCIe2 mailbox, legacy INTx), scans, joins of open and WPA2-Personal networks (CCMP, our own supplicant: 4-way and group key handshakes, keys handed to the firmware); publishes the link as `wlan0` once associated (`net` takes it when there is no `eth0`: DHCP, DNS, TCP) and its control as `wifi` (`wifi status` names the step where bring-up stopped, `wifi retry` runs it again) | the chip sometimes stops answering a few minutes after a join (hardware run 16, cause unknown): `wlan` reports it in `wifi status` and `restart wlan` brings it back. WPA (TKIP), WPA/WPA2 mixed, WPA3-only and networks requiring management frame protection are refused with `EOPNOTSUPP`. Without a PCIe device (emulator) a simulated adapter answers, so `wifi` and the `net` attach are still exercised |

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
| Hardware | UART, I2C, PMIC, RTC, temperatures, screen, touchscreen, USB host, USB device (serial console) | GPU, audio, Joy-Con, buttons (GPIO), frequencies (DVFS), battery |
| Security | capabilities, revocation, Spectre v2, isolation | |
| Tools | build, SD card, tests (`ktest` 27, `stdtest` 18, `wlan` crypto and WPA2 on the PC), debugger (gdb), L4T comparison, USB-C terminal | |

## Limits

- **On a real Switch** (Erista, hekate L4T entry) nxk reaches the
  shell over the USB-C console (`usbcon`, `tools/usbterm.py`): UART-A
  pads are internal. Last hardware run: single core (before PSCI in
  nxk-bl31) and no SD card (see `hwboot/README.md`, "Hardware runs").
  A USB keyboard does not help: its keys go to `input`, not to the
  shell, and hekate's L4T path leaves the xHCI unpowered. Seen there:
  4 cores, SD card and filesystem, `ktest` 27/27, `stdtest` 17/17
  (`hwboot/README.md`, "Hardware runs").
- Kernel messages (`[nxk]` fault reports) go to UART-A and the loader's
  screen only, not to the USB console. Mariko lacks the USB2 OTG device
  controller (its OTG port belongs to XUSB): no USB console there.
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
  (a 500 ms poll stays as a safety net). The chip still stops answering
  at times, a few minutes after a join (`wifi status`: `chip
  unreachable`, with the link and the chip's config space as they are
  then); `restart wlan` brings it back. Its firmware
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
- GPU, audio, Joy-Con and frequency scaling are not done (the last one
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
