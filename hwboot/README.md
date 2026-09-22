# hwboot — physical Switch (Erista, AutoRCM) bring-up kit

Host-side setup to boot a first-generation Switch into **hekate** over USB
(fusée-gelée RCM exploit), then nxk through hekate's L4T entry (Stage 2).
Status: nxk boots to user space on hardware; the shell has not been
reached there yet: the USB-C console ("Shell over USB-C") is the way in,
tried in the emulator only (see "Hardware runs").

## What you need

- Unpatched Erista Switch (first-gen) with AutoRCM enabled: powering it on
  shows a **black screen** and it enumerates over USB as `0955:7321`
  (NVIDIA APX). No jig / Vol+ needed — AutoRCM enters RCM by itself.
- USB **data** cable (charge-only cables fail), ideally USB-A→C into a USB 2.0
  port. Some hubs/docks break RCM; prefer a direct port.
- microSD card (FAT32), inserted in the Switch.
- This machine: gcc, make, curl, unzip, lsusb (`usbutils`).

## Prepare (all done already, re-runnable)

```bash
cd hwboot
./fetch.sh        # hekate RCM payload + Nyx SD pack -> dl/ (pinned v6.5.3)
make -C fusee-nano  # builds ./fusee-nano/fusee-nano (17 KiB, zero deps)
sudo cp 99-switch-rcm.rules /etc/udev/rules.d/ && sudo udevadm control --reload-rules
sudo cp 99-nxk-console.rules /etc/udev/rules.d/ && sudo udevadm control --reload-rules
./mksd-hw.sh /mount/point  # populate the FAT32 microSD (needs it mounted)
```

`mksd-hw.sh` rebuilds nxk, writes `build/nxk.bin` (ELF→raw via llvm-objcopy),
installs hekate + `hekate_ipl.ini` + `bootloader/payloads/nxk.bin` (marker)
and the `/bin` userland on the card.

## Inject (when the Switch is plugged in)

```bash
./check.sh    # exit 0 + APX line = console in RCM and visible
./inject.sh   # sends hekate; the Switch screen should show hekate's menu
```

`inject.sh` uses the vendored `fusee-nano` injector (MIT, see
`fusee-nano/UPSTREAM`). Nothing runs automatically on plug-in by design:
the udev rule only grants permissions, injection stays a manual step.

If `check.sh` sees nothing: reseat the cable, try another port, confirm the
console powers on to a black screen (AutoRCM) rather than the Nintendo logo
(logo = booted into stock OS, not RCM — hold Power 12 s to force off, retry).

## Safety

- RCM + hekate from SD is the standard soft-mod path: it does **not** write
  the Switch NAND by itself. Don't enable emuMMC creation, NAND backup
  restore, or AutoRCM toggles in hekate unless you know what they do.
- Keep the battery charged; a dead battery in RCM looks like a dead console
  (charge for an hour if in doubt).
- nxk DVFS/clock code is emulator-only and power-unsafe: never run
  experimental clock/voltage builds on hardware without review.

## Stage 2: booting nxk via the hekate L4T entry

Neither RCM injection nor `payload=` can boot nxk: it is a ~1.7 MB DRAM
image linked at `0x80000000` (`kernel/link.ld`), not a BPMP/IRAM payload
(~196 KiB ceiling, DRAM uninitialized in RCM).

Chosen chain (validated in the emulator, and on hardware up to the
`display` service):
**hekate `[nxk]` (L4T) → `nxk/bl31.bin` (nxk-bl31 monitor)
→ `nxk/bl33.bin` (= nxk.bin)**.

- `bootloader/ini/nxk.ini`: `l4t=1`, `boot_prefixes=nxk/`, `uart_port=2`
  (hekate enables UART-B pinmux+clock, right Joy-Con rail; monitor and
  kernel still write to UART-A). Shows up under More Configs.
- `nxk/bl31.bin`: built by `mkbl31.py` from `hwboot/nxk-bl31/`
  (Rust `no_std`, ~12 KiB). Loaded by hekate into TZDRAM at `0xFFF00000`,
  it sets on each core the EL3 registers silicon leaves at an unknown
  value (CNTFRQ_EL0, CPUECTLR_EL1.SMPEN, CPUACTLR_EL1), opens UART-A,
  clears the screen hekate left on and copies its logs there, starts the
  system counter, switches the GIC to Group 1, declares its MB as a TZ
  carveout (nxk excludes it via `detect_holes`), copies bl33 from
  `0xAA000000` to `0x80000000`, and jumps to EL2 (SCTLR_EL2,
  MDCR_EL2... in a known state) with x0 = 0 (no DTB: DRAM via
  `MC_EMEM_CFG` + probing). It then serves PSCI 1.0 (`psci.rs`:
  VERSION, FEATURES, CPU_ON, AFFINITY_INFO, SYSTEM_RESET; no
  SYSTEM_OFF, which needs the PMIC). CPU_ON follows exosphere: assert
  the core's CPU/CORE reset, ungate its PMC partition (CE1-CE3), release
  the reset; the core restarts at the monitor's `_start` (SB AA64 reset
  vector, set by hekate and rewritten by the monitor: hekate's lock only
  blocks non-secure writes, which is why nxk in EL2 cannot wake cores
  itself), gets its EL3 and banked GIC setup, then enters nxk in EL2.
- `nxk/bl33.bin`: byte-identical copy of `build/nxk.bin`.
- `bootloader/sys/l4t/` + `libsys_*.bso`: BPMP/SC7/Minerva firmware from
  the Nyx pack (DRAM init by hekate); installed by `mksd-hw.sh` if missing.

Emulator validation (`./test-bl31.sh`, same bl31 binary as on hardware),
two scenarios: without screen (monitor banner, copy, `entry in EL2`,
`PSCI 1.0`, `4/4 cores online`, `nxk>` prompt); then with the screen as hekate
leaves it, reproduced by `fakehekate.py` (DC window A on over a fake
logo at `0xF5A00000`): the image is read back as text, the logo must be
gone and the monitor then kernel lines must appear up to the launch of
`init`. ktest/stdtest through this same monitor:

```bash
NXTEST_QEMU_ARGS="-device loader,file=hwboot/dl/bl31-test.bin,addr=0xFFF00000,force-raw=on \
  -device loader,file=build/nxk.bin,addr=0xAA000000,force-raw=on \
  -global driver=tegra.evp,property=cpu-reset-vector,value=0xFFF00000" \
  ./tools/nxtest.py -c ktest -c stdtest --sd build/sd.img
```

## Screen: what to expect

Without a UART adapter, the screen is the only output: hekate leaves it
on when launching an L4T entry, and the monitor then the kernel write to
it (white text on black, portrait like the hekate text menu: hold the
console vertically) until the `display` service turns it landscape. In
order:

1. hekate logo, then one **line** filling up at the bottom of the
   screen: the hekate countdown (`bootwait`), cleared just before the
   jump to `bl31.bin`. Normal.
2. Black screen, `[nxk-bl31] ...` lines: the monitor is running.
3. `nxk v0.1.0` banner, `[mem]`, `[screen]`, `[boot] ... launching
   init` lines: the kernel is running. A kernel panic or fault shows up
   here too.
4. The `display` service takes over the screen, in landscape (hold the
   console normally): midnight-blue background, the nxk logo centered
   with `AArch64 microkernel for Tegra X1` under it, then one
   `nxk - screen 720x1280 turned landscape, terminal 160x45` line. User
   space is alive. Only `usbcon:` status lines follow (see "Shell over
   USB-C"): service logs and the shell go to UART-A and the USB-C
   console.

Photograph the screen wherever it stops: the last line locates the
failure. If the logo stays on after the line clears (1 without 2), the
monitor did not run or did not find the screen.

## Shell over USB-C (usbcon)

The injection cable doubles as the console. Once nxk runs, the `usbcon`
driver turns the Switch into a USB serial device (CDC-ACM, ID
`1209:0001`, pid.codes test ID) on Erista's OTG controller, the one the
boot ROM uses for RCM, programmed as hekate's `bdk/usb/usbd.c` does. The
PC sees a `/dev/ttyACM*` (`/dev/nxk-console` with the udev rule): keys
typed there reach the shell, and all console output comes back,
including the last 16 KiB printed before the PC attached. Kernel
messages (`[nxk]` fault reports) stay on UART-A and the screen.

1. Leave the USB-C cable in the PC after `./inject.sh`, then boot
   **More Configs → nxk**.
2. Under the nxk logo, the screen shows
   `usbcon: USB-C console ready, waiting for a PC (tools/usbterm.py)`,
   then `usbcon: host detected (bus reset)` and
   `usbcon: configured by the host (high speed)` once the PC has
   enumerated it (`dmesg`: `cdc_acm ... ttyACM0: USB ACM device`).
3. On the PC: `../tools/usbterm.py` (Ctrl-] quits), or
   `../tools/usbterm.py -c ps -c mem` to run commands and exit. Any
   serial terminal works too (`picocom /dev/nxk-console`). The screen
   then shows `usbcon: terminal open on the host`.

The last `usbcon:` line locates a failure:

- `ready` but no `host detected`: the PC saw nothing (`lsusb` has no
  `1209:0001`): PHY setup or cable. Try another port, photograph the
  screen.
- `host detected` without `configured`: enumeration failed; `dmesg` on
  the PC gives the reason (e.g. `device descriptor read/64, error -71`).
- `configured` but `usbterm.py` cannot open the device: permissions
  (udev rule above, or the `dialout`/`uucp` group).
- `fuses unreadable: PHY left without its trims`: hekate did not leave
  the fuses readable; high speed may then fail.

## Hardware runs

1. 22/09/2026, Erista, before the screen console: stage 1 with nothing
   after. nxk then had no output outside UART-A, and the hardware path
   had gaps QEMU hides, all fixed since: EL2/EL3 registers with unknown
   reset values (MDCR_EL2 can trap nxk OSLAR_EL1 accesses to an EL2 with
   no vectors), locks (exclusive accesses) taken with the MMU off,
   hekate GPU carveouts not excluded from free memory.
2. 22/09/2026, Erista, commit `d5e53f6`: **stage 4 reached**,
   `nxk - screen 720x1280, terminal 90x80`. Working on silicon: hekate
   L4T → nxk-bl31 → EL2 entry, single core, DRAM detection, MMU, kernel
   boot, `init`, spawning from the boot image, and `display` taking over
   DC window A with its own DMA image. Not observed: the other services
   and the shell (UART-A only, no way in yet: "Hardware test", step 3).
3. 22/09/2026, Erista, commit `d0d478c`: **shell reached over the USB-C
   console** (`usbterm.py`), stable under load (ktest, stdtest, bench,
   ~8 min, no fault, 30 C). `bench`: null syscall 239 ns, IPC round
   trip 1554 ns, IPC 4 KiB 317 MB/s, thread create+join 6 us,
   spawn+wait 89 us. Failures: single core (ktest affinity and load
   balancing: no PSCI then), and no SD card (`ls /`: ENOSYS, stdtest
   `files`): hekate's `sd_end()` powers the card off before the L4T
   jump, and `sd` expected it on. `usb` and `net` exit (xHCI off).

## Hardware test (once the SD card is back in the PC)

1. `./mksd-hw.sh /mount/point` (rebuilds nxk + bl31, installs the entry).
2. Put the card back in the Switch, inject hekate, **More Configs → nxk**.
3. Read the screen (see above). The shell: over the USB-C cable ("Shell
   over USB-C"). The monitor, the kernel and the `console` service
   otherwise use UART-A, whose pads are internal; a right Joy-Con rail
   adapter (UART-B, 115200 8N1) would only work once they switch to
   UART-B. A USB keyboard does not reach the shell: its keys go to the
   `input` service, and hekate's L4T path leaves the xHCI unpowered
   (`usb` exits).
4. Expected: `entry in EL2`, `PSCI 1.0`, `4/4 cores online`, `launching init`,
   then the `display` service screen and the `usbcon:` lines.
5. Recovery: a failed boot is just a hang. Hold Power 12 s (forced
   off), reboot into RCM, retry. No NAND, eMMC or SD writes by nxk:
   nothing persistent can be corrupted.

## Why nxk.bin can't be injected directly

Reminder: RCM payloads = BPMP in IRAM, ~196 KiB max, DRAM off.
`build/nxk.bin` is ~1.7 MB (kernel + embedded bootfs) and runs at
`0x80000000` in EL3/EL2 with the MMU off. Hence the intermediate
monitor, which does what ATF normally does: secure GIC + EL2 jump.
