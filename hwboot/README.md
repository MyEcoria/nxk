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
   console normally): midnight-blue background, a header with the nxk
   logo, the USB, ETH, WI-FI and SD states, the temperature and the
   uptime, a tab bar at the bottom (HOME, SYSTEM, NETWORK, TERMINAL,
   LOGS) and the HOME tiles: CPU, MEMORY, STORAGE (SD card free space),
   NETWORK and TERMINAL, refreshed every second. User space is alive.
   Driver failures (`usbcon:` or `sd:` lines) and `fbecho` go to the
   LOGS page, whose tab counts the unread ones; `display: 720x1280
   panel, turned landscape` is the first. Service logs and the shell
   go to UART-A and the USB-C console, which the TERMINAL page mirrors.
5. Tap the tabs: each opens its page at once. Nyx powers the panel
   off before chaining to nxk, so `input` powers it up again (reset pin
   J7, LDO6, then a system reset). `[input] touch controller 0x3670,
   firmware ...` then `[input] touchscreen ready (interrupt on GPIO
   X1)` in the boot log mean the controller answered and is read on its
   interrupt line; `touchscreen missing (...)` (also on the LOGS page)
   means it did not answer, the errno says at which step; `touch
   interrupt stuck low, polling instead` means the line never went back
   high (then taps still work, up to 0.5 s late).

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
including the last 16 KiB printed before the PC attached (kernel boot
log and `[nxk]` fault reports too). Output starts 100 ms after the
terminal opens: a fresh ttyACM echoes until its program sets it raw,
and the echo would be typed into the shell.

1. Leave the USB-C cable in the PC after `./inject.sh`, then boot
   **More Configs → nxk**.
2. The TERMINAL page's status line reads `waiting for a PC`, then
   `host seen, not configured` and `connected, high speed, terminal
   closed` once the PC has enumerated it (`dmesg`: `cdc_acm ...
   ttyACM0: USB ACM device`).
3. On the PC: `../tools/usbterm.py`, or
   `../tools/usbterm.py -c ps -c mem` to run commands and exit. Any
   serial terminal works too (`picocom /dev/nxk-console`). The line
   then reads `terminal open, high speed` with the bytes each way.

`usbterm.py` edits lines as zsh does: the line is edited on the PC and
typed whole on Enter (Emacs keys, Up/Down search the history for lines
starting with what is typed, Ctrl-R searches it, `~/.local/state/nxk/`
keeps it), commands are colored as typed (known ones green, unknown red,
quotes and `>` yellow), and a two-line prompt gives the directory, the
last status (`✘ 2 ENOENT`), its duration and the time. Messages that
come while a line is edited print above the prompt; keys typed while a
command runs reach it as typed, and whatever was left unsent comes back
in the next prompt. Ctrl-D on an empty line, `exit` or Ctrl-] quits;
`--raw` is the plain terminal (the shell's `nxk> ` and echo).

The USB-C line locates a failure:

- `waiting for a PC`: the PC saw nothing (`lsusb` has no `1209:0001`):
  PHY setup or cable. Try another port, photograph the screen.
- `host seen, not configured`: enumeration failed; `dmesg` on the PC
  gives the reason (e.g. `device descriptor read/64, error -71`).
- `connected` but `usbterm.py` cannot open the device: permissions
  (udev rule above, or the `dialout`/`uucp` group).
- `console not running`: usbcon stopped; its reason is on the LOGS
  page (`usbcon: ...`). `fuses unreadable: PHY left without its
  trims` there means hekate did not leave the fuses readable; high
  speed may then fail.

## Internet over USB-C (usb0)

The same cable carries a network link: `usbcon` is a composite device,
the console (CDC-ACM) plus a CDC-ECM network function that the PC's
`cdc_ether` driver binds (macOS has one too, Windows none). On the
Switch it is link `usb0` (MAC `02:4e:58:4b:00:01`, the PC's end
`02:4e:58:4b:00:02`); `net` prefers it to Wi-Fi once the PC turns it
on.

1. With the udev rule (reinstall it if it predates `nxk0`) the PC's
   interface is `nxk0`, else `enx024e584b0002`.
2. Share the PC's connection once, as a phone hotspot does
   (NetworkManager answers DHCP at 10.42.0.1 and routes the Switch):
   `nmcli connection add type ethernet ifname nxk0 con-name nxk-share
   ipv4.method shared ipv6.method ignore`.
3. NETWORK page: USB-C ETHERNET `shared by the PC`, ACTIVE; INTERNET
   `10.42.0.x` via usb0. In the shell: `nettest ping 10.42.0.1`, then
   `nettest ping google.com`.

The card locates a failure:

- `no DHCP: sharing off on the PC?`: the link is up but nothing
  answers DHCP (step 2 missing). After 10 s `net` moves to Wi-Fi if
  it is joined, until the cable or the PC's link goes down and up.
- `link not turned on by the PC`: no driver took the network function
  (`dmesg` on the PC should show `cdc_ether ... register 'cdc_ether'`).
- `[usbcon] controller reports N endpoints` in the boot log with N
  below 5: the controller lacks the network endpoints.

## Driver updates over USB-C (no SD swap)

A boot driver can be replaced while nxk runs, with the same hardware
grants, from a build on the PC:

```bash
./tools/build.sh
./tools/usbterm.py --reload wlan -c "wifi status"
```

`--reload wlan` copies `~/.cache/nxk/target-user/.../wlan` to
`/bin/wlan` on the card (`--put LOCAL:REMOTE` for any file: the shell's
`recv` takes base64 lines, checks the CRC-32 and writes the file), then
types `restart wlan`: init kills the running instance, unpublishes its
services and starts the file with the driver's grants. Only the console
shell may ask for it. The kernel, init, the shell and the boot image
still need a reboot: either `mksd-hw.sh`, or, with the card working,
`--put build/nxk.bin:/nxk/bl33.bin` together with a `bl31.bin` rebuilt
for it (`mkbl31.py`): the monitor copies the kernel with the size baked
into it, and a larger kernel loaded by an older `bl31.bin` loses its
tail (run 18: a kernel panic in `objtab.rs`). A 2 MB kernel takes about
5 s (each 512-character line is echoed back before the next).

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
   lane power registers, `IO_DPD_STATUS` and PCIEXCLK.
8. 23/09/2026, Erista, commit `68d5c34`: **PCIe link up on port 1**
   (lane 0, the x1 port) and a Broadcom endpoint behind it: `wifi
   status` reads `adapter BCMffff rev 15 on PCIe port 1`. Config reads
   reach the chip, memory reads through BAR0 come back all ones: the
   root port had bus numbers but no memory window and no memory
   decoding, so it forwarded nothing. Fixed since (`pci_setup_bridge`
   equivalent); an all-ones chip ID now also reports the AFI error
   code and address.
9. 23/09/2026, Erista, commit `d5c821d`: **Wi-Fi chip identified**,
   `adapter BCM4356 rev 3 on PCIe port 1, firmware present` (rev 3:
   the A3 silicon the console's `4356a3` firmware is built for). The
   whole bring-up works on silicon: PMIC rail, module pins, PCIe link,
   bridge windows, backplane reads through BAR0. What remains is the
   data path: firmware download and Broadcom's msgbuf rings.
10. 23/09/2026, Erista, commit `263ea9d`: driver updates over USB-C
    work (`usbterm.py --reload wlan`: upload, CRC check and restart in
    7 s). They showed that only the first bring-up after power-on
    worked: a restarted `wlan` saw a stale link whose config reads
    returned all ones, or, with the root complex reset, no link at all.
    Holding the module off longer changed nothing; recalibrating the
    UPHY PLL, which still read locked, did. Each start now resets the
    root complex through `clk` and calibrates the PLL from scratch;
    repeated restarts identify the BCM4356 every time.
11. 23/09/2026, Erista, commit `8f18e6e` plus `wlan` over USB-C:
    **the Wi-Fi firmware runs**. The enumeration ROM lists 9 cores
    (ChipCommon rev 47, 802.11 rev 48, ARM CR4 rev 6, PCIe2 rev 11,
    ...); the console's own firmware, copied into the TCM with its
    NVRAM, publishes its shared area (version 5, flags 0x20005) 200 ms
    after the ARM is released, and its console reads `wl0: Broadcom
    BCM4356 802.11 Wireless Controller 7.35.252.108`.
12. 23/09/2026, Erista, `wlan` over USB-C: **`wifi scan` lists the
    networks around** (about 40, 2.4 and 5 GHz, signal, security and
    BSSID). The msgbuf rings work first time: five common rings in
    uncached DMA memory, indices in the TCM, doorbell through the PCIe2
    mailbox, completions polled with their sequence numbers checked.
    The firmware answers `ver` (`version 7.35.252.108`, built 29 Aug
    2021) and `cur_etheraddr` (00:90:4c:1a:a0:01, Broadcom's
    placeholder: nxk sets its own address), then scans with `escan`.
13. 23/09/2026, Erista, `wlan` over USB-C: **first join**, to an open
    phone hotspot (`wifi connect nxk-test`: `state connected`, channel 6,
    -33 dBm; disconnect and rejoin work too). The same hotspot on WPA2
    failed at once, `sup_wpa: firmware error -23` (`BCME_UNSUPPORTED`):
    the firmware's `cap` list has no `idsup`, so it has no supplicant
    of its own, and even `sup_wpa 0` is refused. `wlan` now leaves
    `sup_wpa` alone on open networks and refuses WPA/WPA2 with
    `EOPNOTSUPP` until a host supplicant exists.
14. 23/09/2026, Erista, `wlan` over USB-C: **first WPA2 join**, to the
    same phone hotspot on WPA2-Personal (`wifi connect nxk-test
    PASSPHRASE`: `state connected`, channel 7, -33 dBm, still up after
    30 s; a wrong passphrase ends in `key handshake refused` and
    `EACCES`). `wlan` now runs the 4-way handshake itself. The data
    rings came first (64 RX buffers taken, `rxpost 64/64`), yet no frame
    ever completed on them: logging every firmware event showed the
    access point's message 1 arriving as `E_EAPOL_MSG` (event 25, the
    99-byte EAPOL PDU, sender in the event header). Replies leave on the
    TX flow ring (created on the first frame, 2 sent, 2 completed), then
    the pairwise and group keys go in with `wsec_key`. A boot-image
    `wlan` idle about 3 minutes had lost the chip again (`ring 2 write
    index 0xffff`); a restart brought it back.
15. 23/09/2026, Erista, `wlan` over USB-C: **traffic over Wi-Fi**.
    `wlan0` published once joined; `net`, waiting since boot for a link,
    took it and got `10.133.165.75/24` by DHCP from the phone hotspot
    (gateway and DNS `10.133.165.217`). `nettest resolve example.com`
    and `nettest get example.com 80 /` worked (HTTP 200, 829 bytes in
    221 ms). Data frames arrive on the RX completion ring (only EAPOL
    comes as events). With the rings polled every 2 ms while joined,
    `wlan` used 99 ms of CPU in 10 s idle.
16. 23/09/2026, Erista, `wlan` over USB-C: **chip interrupts**. The
    PCIe2 mailbox raises INTx, on the AFI's line (SPI 98) `wlan` already
    held; the rings are drained on it, a 500 ms poll as a safety net.
    Idle while joined `wlan` now uses no measurable CPU (99 ms per 10 s
    with the 2 ms poll); an HTTP GET takes 160 to 270 ms, 60 in a row
    and 10 idle minutes went fine. Two driver bugs showed up and are
    fixed. Reading right after the interrupt caught completions before
    their DMA landed: the reader expected sequence numbers this firmware
    leaves at 0 on control completions, so items are now cleared once
    read and one still reading type 0 waits. And `wifi status` on a chip
    that had stopped answering read its whole 64 KiB console a byte at a
    time, each read stalling about 0.65 ms: `wlan` spent 42.9 s of CPU
    there, then died (the "43 s of CPU" of commit `e5f7c9e`). It now
    reads the last 600 bytes, and nothing once the chip is gone.
    Still open: in about half the runs with `net` attached, the chip
    stopped answering one to a few minutes after the join, with the link
    still up at the root port (DL active), no mailbox data, no deep sleep
    request and no AFI error: the chip itself resets or hangs. Once the
    completion rings stalled instead (transmit completions at 110 of
    142, receive at 131 of 140), with 16 control messages of an unknown
    type (0x1D) and a received frame read 16 bytes late, as if the chip's
    DMA writes landed 16 bytes low. `wifi status` now shows the link, the
    chip's config space once it is gone, and the next unread items.
17. 23/09/2026, Erista, boot image from `mksd-hw.sh`: **the Switch
    pings google.com over Wi-Fi**. The first image carrying the full
    Wi-Fi driver did not mount the card: `fs` gave up its 5 s wait for
    `sd`, then, waiting longer, its first read timed out. `sd` now
    reports a failed transfer: `status 0x00000000, present 0x01ab0206,
    ADMA error 0x0 at 0x80380008, buffer 0x80380000`, a read active with
    the data lines at 1010 and the DMA engine fine. hekate runs the card
    in 1.8 V UHS signaling, which only a power cycle undoes; `sd`
    assumed a card switched off and powered it up at 3.3 V, where
    commands pass and data does not. It now powers the slot off, waits
    100 ms, then up: the card mounts, `wlan` loads its firmware at boot
    (after `fs`, then the PCIe bring-up), joins `nxk-test` (WPA2), `net`
    gets `10.133.165.75/24` by DHCP and `nettest ping google.com 5`
    (`172.217.22.46`) answers 5 of 5, 37 to 301 ms, 120 ms on average.
18. 24/09/2026, Erista, the cleanup up to commit `6c563fb`: kernel log
    ring, interrupt-driven `console`, single-thread `usbcon`, the home
    screen. `ktest` 29/29 (the kernel log costs no UART wait; long
    zeroing lets the other cores in), `stdtest` 17/17 (`files` 133 ms).
    `bench`: null syscall 239 ns, IPC same core 1598 to 1600 ns, cross
    core 10.1 to 10.2 us, spawn+wait 92 us; it now pins its own thread
    (it read 1.6 or 10 us same core, depending on where it ran).
    `console` used 5 ms of CPU in 71 s (696 ms in the morning, waiting
    on UART-A byte by byte), `usbcon` 1 thread and 10 pages (2 and 19),
    and a 2 MB `--put` took 5.3 s instead of 237 s. Kernel reports now
    reach the USB console, which showed `usb` dying at every boot on an
    alignment fault reading its registers (exit 139, the zombie of
    earlier runs). A fresh ttyACM echoes: the history sent the moment
    the PC opened it came back as keystrokes and ran as commands, hence
    the 100 ms hold. The SD card was mute on 6 of 8 boots: it answered
    every command, CMD13 in state 5 (data), yet no data came (lines at
    1010), with the host at 3.3 V as intended, LDO2 at 3.3 V and the
    power switch on, as in run 17. The image of the morning failed the
    same way (control boot), and hekate's 239 ms power-off did not cure
    it; the card read fine on the PC every time, and in nxk at the first
    and last boots. `sd` now reads block 0 before publishing and retries
    at 1.8 V, after a 2 s power cycle, then on a 1-bit bus, reporting
    each (not exercised yet). A kernel deployed with an older `bl31.bin`
    panicked (`objtab.rs:57`, index 3759): the monitor copies the size
    baked into it.
19. 24/09/2026, Erista, commits `f80c073` to `e1b9bb1`: `usb` now stops
    cleanly, `[usb] xHCI registers unusable (CAPLENGTH 0xff, HCIVERSION
    0xffff, HCSPARAMS1 0xffffffff)` with no exit 139; `ktest` 29/29 three
    times, priority preemption now judged against the timer lateness.
    The SD card sent no data on all 4 boots (first read into `sd`'s own
    page, DAT lines idle high; the retries then fail with EIO and CMD13
    stops answering), so `fs` found no volume, `stdtest` and the direct
    DMA of `sd` went untested. hekate explains it: "SD Card Issues
    Warning", the card initialized in 1-bit mode (microSD page: initial
    bus HS25 1bit, 3 init fails, no read/write errors after), with this
    64 GB SanDisk and with a second card: the Switch's slot carries DAT0
    only (DAT1-3 contact), so the 4-bit bus `sd` uses never gets data,
    which likely explains the mute card of run 18 too. Both cards work
    on the PC; a Realtek reader (0bda:0177) timed out on every write,
    the ChipsBnk one formatted the second card.

20. 24/09/2026, Erista, commit `f2804ee`, the Switch's microSD connector
    reseated (it was loose: the cause of run 19): the card comes up on 4
    bits (`sd` ready at 766 ms, no retry), `fs` mounts it and `wlan`
    loads its firmware from it. `ktest` 29/29, `stdtest` 17/17 (`files`
    133 ms), 9996 KiB of memory in use after the mount. The direct DMA
    of `sd` holds: a 2 MB `--put` (3.8 s, 5.3 s in run 18), a `cp` of it
    on the Switch (over 1 MB read back from the card) and `/bin/wlan`
    from `--reload` are all byte-identical on the PC. The Wi-Fi chip
    reset itself about 3 minutes after boot, idle and before any join
    (config space back to its reset values, command 0x00100000, BAR0
    0x4, the link still up); `--reload wlan` brought it back, `wifi
    connect nxk-test` (WPA2) joined, and once DHCP had answered (the
    first ping, 3 s after the join, found no address: ENETUNREACH)
    `nettest ping google.com 5` got 5 of 5 (172.217.22.46, 66 to 253 ms,
    109 ms on average), 3 of 3 again 2.5 minutes after the join.
21. 24/09/2026, Erista, the Wi-Fi fixes (`2988255` to `63545b5`):
    `wifi connect nxk-test` (WPA2) now returns with the DHCP address
    (10.133.165.75/24) and `nettest ping google.com 5` answers 5 of 5
    at once (124 ms on average). `wifi retry` resets the chip on demand:
    report, module reset, firmware up in 194 ms, rejoin by itself, 5 of
    5 pings (55 ms). A loss of its own came 52 s later, caught within a
    second: the PCIe link down at the root port (DL_ACTIVE and DL_UP
    clear), the config space reading all ones, WIFI_EN, WIFI_RST and
    PERST# held high by the Tegra, no trap or assert in the shared area
    and nothing new on the firmware console. `wlan` brought it back and
    rejoined, and the next 11 minutes (pings every 30 s) had no loss.
    So the chip resets itself (a watchdog or a supply dip, not the
    host): runs 16 and 20 looked later, once the link had retrained
    and the config space was back to its reset values.

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
