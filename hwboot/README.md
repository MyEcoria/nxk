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
./mksd-hw.sh /mount/point ~/brcm  # ... plus Wi-Fi blobs -> /firmware/brcm
```

`mksd-hw.sh` rebuilds nxk, writes `build/nxk.bin` (ELF→raw via llvm-objcopy),
installs hekate + `hekate_ipl.ini` + `bootloader/payloads/nxk.bin` (marker)
and the `/bin` userland on the card. A second argument (or `NXK_FIRMWARE`)
copies a directory of Wi-Fi blobs to `/firmware/brcm`, where `wlan` looks
for `brcmfmac4356-pcie.bin` and `brcmfmac4356-pcie.txt`: proprietary files
that are never part of the repository.

## Wi-Fi firmware (extracted from the console)

The `wlan` service needs Broadcom's firmware plus the calibration data
of the board. Neither is redistributable: the image is absent from
linux-firmware, and both live in the console's own system titles
`0100000000000016` (wlan) and `010000000000000B` (bluetooth). They are
extracted from the console that will run them, into `firmware/`, which
git ignores.

1. Keys: hekate, `Payloads` -> `Lockpick_RCM`; it writes
   `sd:/switch/prod.keys`.
2. ExeFS: hekate, `Launch` -> Atmosphere, then `nxdt_rw_poc.nro` (the
   nxdumptool rewrite: the 1.1.x series does not read system titles) ->
   `system titles menu` -> `wlan` -> `Program #0` -> its PartitionFS
   section, with `write raw section` off. The result lands in
   `sdmc:/nxdt_rw_poc/NCA FS/System/Extracted/.../Program #0/0/main`.
3. Firmware: `./hwboot/nxfw-extract.py wlan <that main> -o firmware`,
   and the same with `bluetooth` for `BCM4356A3.hcd`.
4. Card: `./hwboot/mksd-hw.sh /mount/point firmware` copies them to
   `/firmware/brcm`, where `wlan` looks for them.

`main` is the title's NSO image and `nxfw-extract.py` reads it directly,
so neither hactool nor nx2elf is needed; the markers it looks for come
from nx-fwextract (perillamint, GPL-3.0). What comes out:

| File | Content |
|---|---|
| `brcmfmac4356-pcie.bin` | firmware, ~570 KiB, version `4356a3-roml/pcie-ag-msgbuf-...` |
| `brcmfmac4356-pcie.txt` | NVRAM of the board (`boardtype=0x074a` on an Erista) |
| `BCM4356A3.hcd` | Bluetooth firmware, unused so far |

That version string is also the answer to two design questions: the chip
talks over PCIe, and its data path is Broadcom's msgbuf ring protocol.
The sample NVRAM files that circulate for the BCM94356Z module describe
another board (`boardtype=0x073e`): use the console's own.

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
   Both failures are fixed since (PSCI `CPU_ON` in the monitor, slot
   power-up in `sd`).
4. 22/09/2026, Erista, commit `e35837f`: **4 cores and the SD card on
   silicon**. `ktest` 27/27 (affinity, migration and load balancing
   included), `stdtest` 17/17 (`files` on the card: 180 ms), `ls /`,
   `df` (60890 MiB) and `run /bin/hello-std` from the card. `bench`:
   null syscall 239 ns, IPC same core 1556 ns, cross core 9774 ns
   (a real IPI now), IPC 4 KiB 314 MB/s, thread create+join 15 us,
   spawn+wait 90 us. Two hardware-only lessons on the way: the masked
   GPIO registers (0x80+) did not take, hekate's read-modify-write on
   the plain ones does, and `sd` touching SDMMC1 before its clock was
   enabled stalled the APB bus, which killed `usbcon` with it.
5. 23/09/2026, Erista, commit `54686c7`, firmware on the card: Wi-Fi
   stopped at its first step, `wifi status` reading
   `PCIe rail (PMIC LDO1): ETIMEDOUT`. `power` and I2C were fine
   (`temp`, `date`); `wlan` had given up its 2 s wait for `power`.
   Cause: UART-A drained 1.81 ms per byte (~5.4 kbaud, a ~19.2 MHz
   clock under a divisor computed for PLLP), and every service's log
   line waits for `console` to push it out, so the boot log alone
   delayed `power` past 2 s (`console` spent 8.8 s of CPU printing
   about 5 KB of shell output). An emulator console slowed to that rate
   reproduces the same message. Fixed since: nxk-bl31 and the kernel
   select PLLP for UART-A (hekate's `clock_enable_uart`), `wlan` waits
   5 s and `wifi retry` reruns the bring-up without a reboot.
6. 23/09/2026, Erista, commit `795fce0`: UART-A at full speed
   (`console`: 247 ms of CPU after 44 s, against 3.8 s after 97 s),
   and `wlan` past the rail, the clocks and the root ports, stopping at
   `no link on either port`: `RP_VEND_XP` 0x19800000 on both ports
   (data link down), UPHY lane mux 0x817fc0fe (lane 0 x1, 1-4 x4, 5-6
   USB3). The PEX pads (PERST#, CLKREQ#, WAKE#) and the module GPIO
   pads were left as the boot chain set them, PARK bit included, which
   Horizon clears on every pad (Atmosphère `pinmux_platform_pads.cpp`)
   and hekate before driving a GPIO. Fixed since: `wlan` applies
   Horizon's Icosa values to those seven pads, lifts PERST# 100 ms after
   the module powers up, and a failed link reports pads, PH pins and
   port registers in `wifi status`. The BAR window also grew to 8 MiB
   for the chip's 4 MiB BAR2.
7. 23/09/2026, Erista, commit `6144ac3`: the readback confirmed run 6.
   WIFI_EN/RST were `0x74` (parked, tristated: no earlier build ever
   drove them) and the PEX pads `0x460`/`0x470` (parked); all now as
   Horizon sets them, PH0/PH1 driven high, PERST# released and REFCLK
   on (`AFI_PEX_CTRL` 0x1b), UPHY PLL locked. Still no link on either
   port. Next suspects, all from Linux's T210 sequence and changed
   since: PCIEXCLK was released before the AFI crossbar and port
   enables were programmed (now held across them via `clk`
   `PCIE_LINK`), the UPHY AUX mux clamps in `ELPG_PROGRAM1` were never
   released, and the PEX bias/REFCLK pads may sit in PMC deep
   power-down (`clk` lifts it). The report adds `ELPG_PROGRAM1`, the
   lane power registers, `IO_DPD_STATUS` and PCIEXCLK. Not re-run on
   hardware yet.

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
   then the `display` service screen and the `usbcon:` lines. In the
   shell: `cpu` shows the four cores, `ls /` the SD card.
5. Recovery: a failed boot is just a hang. Hold Power 12 s (forced
   off), reboot into RCM, retry. No NAND, eMMC or SD writes by nxk:
   nothing persistent can be corrupted.

## Why nxk.bin can't be injected directly

Reminder: RCM payloads = BPMP in IRAM, ~196 KiB max, DRAM off.
`build/nxk.bin` is ~1.7 MB (kernel + embedded bootfs) and runs at
`0x80000000` in EL3/EL2 with the MMU off. Hence the intermediate
monitor, which does what ATF normally does: secure GIC + EL2 jump.
