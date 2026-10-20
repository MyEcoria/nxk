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
../tools/gm20b-fw.sh              # GPU firmware from linux-firmware -> firmware/nvidia
./mksd-hw.sh /mount/point ../firmware  # Wi-Fi blobs and firmware/nvidia -> /firmware
```

`mksd-hw.sh` rebuilds nxk, writes `build/nxk.bin` (ELF→raw via llvm-objcopy),
installs hekate + `hekate_ipl.ini` + `bootloader/payloads/nxk.bin` (marker)
and the `/bin` userland on the card. A second argument (or `NXK_FIRMWARE`)
copies a directory of Wi-Fi blobs to `/firmware/brcm`, where `wlan` looks
for `brcmfmac4356-pcie.bin` and `brcmfmac4356-pcie.txt`: proprietary files
that are never part of the repository. Its `nvidia/` subdirectory, which
`tools/gm20b-fw.sh` fills from the PC's `/lib/firmware/nvidia/gm20b`
(NVIDIA's redistributable GPU firmware, the set nouveau loads), goes to
`/firmware/nvidia` for `gpu`.

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
   `/firmware/brcm`, where `wlan` and `bluetooth` look for them.

`main` is the title's NSO image and `nxfw-extract.py` reads it directly,
so neither hactool nor nx2elf is needed; the markers it looks for come
from nx-fwextract (perillamint, GPL-3.0). What comes out:

| File | Content |
|---|---|
| `brcmfmac4356-pcie.bin` | firmware, ~570 KiB, version `4356a3-roml/pcie-ag-msgbuf-...` |
| `brcmfmac4356-pcie.txt` | NVRAM of the board (`boardtype=0x074a` on an Erista) |
| `BCM4356A3.hcd` | Bluetooth patch RAM, loaded by `bluetooth` over UART-D |

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
   logo, the USB, ETH, WI-FI, SD and FAN states, the temperature and
   the uptime, a tab bar at the bottom (HOME, SYSTEM, NETWORK, TERMINAL,
   LOGS) and the HOME tiles: CPU, MEMORY, STORAGE (SD card free space),
   NETWORK and TERMINAL, refreshed every second. User space is alive.
   Logs (the kernel's, the services' such as `[usbcon]` or `[sd]`
   lines, and `fbecho`) go to the LOGS page, whose tab counts the
   unread ones; the TERMINAL page shows the shell's session only, the
   commands typed and their results. Both also go to UART-A and the
   USB-C console.
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
in the next prompt. `clear` is the client's own: it wipes the PC's
screen and scrollback, nothing reaches the Switch, and shows the logo
header again. Ctrl-D on an empty line, `exit` or Ctrl-] quits;
`--raw` is the plain terminal (the shell's `nxk> ` and echo). Logs are
left out, as on the TERMINAL page: `--logs` shows them (dimmed), and
`--reload` does too, to see the driver start.

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

## Debug channel: screen capture, touch and gamepad

The same serial stream carries a side channel that lets the PC (or an
agent) see the screen and act on it, without touching the console:
`usbcon` takes frames out of the shell's input (APC strings, `ESC _ N`,
base64 of a request, `ESC \\`, which terminals ignore) and puts its
replies in the output between two console transfers. `nxk_abi::proto::dbg`
has the requests; `tools/nxdbg.py` is the client (close `usbterm.py`
first: one program per port).

```bash
./tools/nxdbg.py ping
./tools/nxdbg.py shot screen.png            # the screen as held, landscape PNG
./tools/nxdbg.py tap 640 670                # landscape pixels (1280x720)
./tools/nxdbg.py swipe 1000 300 200 300 400 # x1 y1 x2 y2 [ms]
./tools/nxdbg.py button A                   # gamepad: A B X Y L R ZL ZR PLUS MINUS UP DOWN LEFT RIGHT HOME ...
./tools/nxdbg.py tap 640 670 wait 0.5 shot after.png   # chained, in order
```

A capture is the image `display` shows (`display::SNAPSHOT`, read-only),
cut into 32-bit pixel runs by `usbcon` as the IN endpoint frees up: about
0.09 s for a whole screen, 30 in a row in 2.7 s. Touches go to `input`
as the panel's raw coordinates (`input::INJECT`), buttons as `KIND_BUTTON`
events. `display` reads both: the D-pad moves an outline over the tappable
zones (the panel and the confirmation dialogs restrict it to their own),
A taps the outlined one, B closes a panel or goes back, L and R (or ZL
and ZR) turn the page, HOME and + go to the home page. A finger on the
screen hides the outline.

Frames only flow while a terminal is open on the host (DTR), and a
lone ESC typed in the shell reaches it with the next key. `--reload` of
`usbcon`, `display` or `input` replaces them in place; the PC's port
disappears and returns for `usbcon`.

## Games

The library (GAMES, the first page of `display`) lists `/games/NAME`
directories: the program `NAME`, `game.txt` (`title=`, `publisher=`,
`version=`, `description=`, `color=RRGGBB`) and `cover.bin` (its art,
`tools/mkcovers.py`), re-read every 8 s; the time played goes to
`/users/ID/playtime.cfg` of the user playing (users and their save
data: `/users/ID/`, README "Users and save data"). `mksd-hw.sh` installs the ones built from
`jeux/`; for a card in the console,
`./tools/usbterm.py --put LOCAL:/games/NAME/NAME` (the directory first:
`-c "mkdir /games/NAME"` in an earlier call).

Console buttons (`input`, GPIO X6 and X7, active low, interrupt on both
edges): Volume+ is LEFT, Volume- is RIGHT, both together are A (a lone
button counts after 120 ms, so a chord does not move first). The port
reads 0xC0 at rest on the Switch; **not yet checked with a finger on the
buttons** (the pins are hekate's `btn.c`'s, from memory: `[input] volume
buttons: ...` lines on the LOGS page show each press). The Power button
is the PMIC's (MAX77620 ONOFFSTAT bit EN0 on I2C5, as hekate's `btn.c`),
polled every 100 ms: a short press puts the console to sleep (it wakes
on the next press or touch), held 3 s it opens the Power sheet (Sleep
Mode, Restart, Power Off). **Not yet checked on hardware** (`[input]
power button: pressed/released` lines on the LOGS page); in the
emulator, `nxdbg.py button POWER` and `POWER_LONG` stand in for it. A
game or program owning the screen ignores it.

Gamepad (or `nxdbg.py button`): left and right choose a card, A plays;
touch: a tap chooses, a second tap plays. In a game `display` hands out
the input (`display::EVENT`): touch and the same buttons, HOME ends the
game (the library comes back; a game that exits or crashes gives the
screen back too, and so does one run from the shell). On entering a
game the GPU is put on the highest clock the power source allows
(768.0 MHz on a charger, 460.8 on battery). Screen captures of a game
(`nxdbg.py shot`) come from a copy of the image on display.

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

## Bluetooth controllers

`bluetooth` starts at boot (init's driver list): the first run needs a
boot image (`mksd-hw.sh`, or `--put` of `bl33.bin` with its `bl31.bin`
as in "Driver updates over USB-C"), with `BCM4356A3.hcd` in the
firmware directory (see "Wi-Fi firmware"); run 36 is the first one on a
Switch. Then, over the USB-C console:

```text
bt status     # state ready, adapter BCM4356A? ..., BCM4356A3.hcd loaded, ... at 3000000 baud
bt pair       # 30 s: hold the controller's sync button
bt list       # paired controllers, connected or not, battery
```

The sync button is the small one on a Joy-Con's rail side, between SL
and SR, and on top of a Pro Controller next to its USB-C port; the
player lights sweep while it looks for a console, and the first one
stays lit once paired. A paired controller reconnects when one of its
buttons is pressed. System Settings, Controllers does the same from the
screen (Pair a Controller, Forget). Keys and the adapter's address are
in `/config/bt.cfg`; `bt forget all` drops them (unpair on the
controller side by pairing it with another console).

When `bt status` says no adapter, its line names the step (no answer
to the reset, a patch record refused, no answer after the patch at any
speed or BT_WAKE level) and dumps what to look at: the RX / CTS pad
levels, the PMIC's 32 kHz output, the UART registers and the first bytes
received, the pads' pinmux and GPIO H. A Hardware Error from the core
(at boot or later) power-cycles it again after 1 s, then up to 30 s
apart for two minutes; `bt status` says when (run 45).
`bt retry` power-cycles the core and starts again;
after a change, `./tools/usbterm.py --reload bluetooth -c "bt status"`.

## Fan

Nyx stops the fan, turns the 5V regulators off and shuts the TMP451
down before chaining to nxk: until `fan` and `power` start, the SoC
has no cooling and the last temperature reading stays frozen. Expected
in the boot log: `[power] temperature sensor was shut down, started`
(nothing if it was running), then `[fan] ready: SoC NN.N C, duty N%`;
`[fan] no fan control (ERRNO)` means the PWM block could not be
clocked, the fan stays off. At the shell:

```text
fan             duty, mode, SoC temperature, speed measured over 1 s
fan 100         full speed by hand: audible, about 11000 rpm
fan 0           stopped (unless the SoC is at 60 C or more)
fan auto        back to the temperature steps
bench burn 300  all 4 cores at 100% for 5 min: SoC and fan every 10 s
```

On the screen, the header's FAN chip or the fan row of SYSTEM opens
the FAN panel: AUTO or MANUAL, then -10% / +10%; every boot starts
in auto. The header's FAN square is green while it spins, yellow when
forced (SoC hot, sensor lost), grey when stopped. `fan` reading 0 rpm while
audible points at the tachometer (GPIO S7, bank 4 interrupt); silent
at 100% points at the 5V rail (GPIO A5) or the PWM pad (V4).
`./tools/usbterm.py --reload fan` replaces the driver in place.

## CPU clock

`cpufreq` moves the CPU between sys-clk's steps on PLLX and sets the
CPU rail (MAX77621) to NVIDIA's PLL-source voltage for this chip's
speedo; steps needing more than 1225 mV are refused (the DFLL would be
needed). Every boot stays at hekate's 998.4 MHz, 950 mV. Expected in
the boot log: `[cpufreq] ready: 998 MHz, 950 mV, speedo NNNN, steps up
to NNNN MHz`; `[cpufreq] boot clock only: WHY` otherwise. At the shell:

```text
cpufreq         clock, rail, speedo and the steps this chip runs
cpufreq 1224    a step (rail raised first): cpu 1219.2 MHz, rail ~1.2 V
bench burn 60   GFLOP/s scale with the clock; no crash, no fault
cpufreq boot    back to 998.4 MHz, 950 mV
```

Each change logs `[cpufreq] PLLX NNNN MHz, measured NNNN MHz`: the
CPU's real speed, from a timed loop against PLLP's 408 MHz. PLLX is
only turned off once that loop shows the CPU slowed down to PLLP;
otherwise `CPU not slowed by PLLP (A -> B ticks): PLLX kept, no
change` and EIO, the rail back where it was. Check each step with
`bench burn` (a wrong voltage shows as a hang, a fault or a GFLOP/s off
the clock ratio). Above the boot clock the
service returns to it when the SoC reaches 80 C (`[cpufreq] SoC at
NN.N C: back to the boot clock`). A hang during a change: reboot, the
boot clock is back. `./tools/usbterm.py --reload cpufreq` replaces the
driver in place (it restarts at the boot clock only after `cpufreq
boot`: the loader's values are read when it starts).

## GPU

`gpu` leaves the GM20B off until asked (`gpu on`, or a 2D or compute
request); a boot changes nothing. Its firmware must be on the card
(`tools/gm20b-fw.sh`, then `mksd-hw.sh /mount/point firmware`,
"Prepare"). Power-up goes through these stages, the ones `gpu` names
on a failure: rail (MAX77621 at 0x1C on I2C5, its EN pin on the PMIC's
GPIO6, NVIDIA's voltage for 76.8 MHz plus 25 mV: 868.75 mV on an
Erista), clocks (`clk`: GPU and PLLG_REF gates, PLLP_OUT5 at 204 MHz,
clamps off with the reset held, then the chip ID), PLL (GPCPLL in
noise-aware mode at 76.8 MHz), MMU (GMMU, BAR1), firmware (files
read), secure boot (NVIDIA's ACR loader on the PMU copies FECS into the
GPU firmware carveout hekate's L4T entry locked), FECS (GR init, GPCCS
loaded, both started), context (golden image) and channel (runlist,
first fenced batch). Expected at `gpu on`:

```text
[gpu] rail 868 mV (GPU speedo NNNN, ID 3)
[gpu] GPCPLL M 1 N 36 PL 9: 76800 kHz (DFS coeff NN, ext cal NN)
[gpu] WPR 0xNNNNNNNN-0xNNNNNNNN, FECS image 23808 bytes
[gpu] ACR loader: production keys, IMEM NN KiB, mailbox 0x0 0xN, NNN us
[gpu] 1 GPC, 2 TPC, 2 LTC
[gpu] FECS up: context NNNNNN bytes, zcull NNN, perfmon NNN
[gpu] golden context NNNNNN bytes, N PBDMA, runlist 0
[gpu] ready in NNN ms
```

Then at the shell:

```text
gpu             gpu on: chip 0x12b000a1, 76.8 MHz, rail 868 mV, speedo NNNN (ID 3)
gpu test        2D fill and copy, fill / copy / SAXPY kernels, 3D clear,
                a triangle, depth ordering, a lit indexed quad and a
                textured quad (nearest and linear): 10/10 ok
gpu bench       FFMA on both SMs for 1 s: at most 39.3 GFLOP/s at 76.8 MHz
gpu clock 768   [gpu] GPU 76800 -> 768000 kHz at 987 mV, measured 768292 kHz
gpu sweep       gpu bench at each step up to the cap, ~77.5% of peak each
bench full      CPU burn and GPU FFMA together, 30 s
gpu load        gpu load: N% over 1000 ms (NNN ms busy since the service started)
gpu off         clamps, reset, clocks and rail off
```

`gpu clock` takes sys-clk's steps (76, 153, ... 921) while the GPU runs,
or keeps one for the next power-up. The rail follows NVIDIA's voltage
for the step and this chip's GPU speedo, plus 25 mV (868.75 mV up to
537.6 MHz, 1068.75 mV at 921.6 MHz with speedo 2081), raised before the
PLL moves and lowered after. Each change is measured (SM cycle counter
against the CPU's clock); more than 5% off goes back to 76.8 MHz with
EIO. Caps as sys-clk's on an Erista, from the charger (BQ24193 power
good): 460.8 MHz on battery, 768 MHz on input power; `force` goes past
them (844.8 and 921.6 MHz: official charger or dock, which nxk does not
tell from a PC port yet). The cap follows the power source every 2 s,
and at a SoC of 80 C the clock goes back to 76.8 MHz.

A failed power-up says where (`gpu failed at stage N (NAME): ERRNO`)
and leaves the GPU off; the `[gpu]` lines before it tell which step
answered what. The ACR loader's mailbox 0 must read 0: another value
is its error code, and a missing `WPR` range (0x0-0x20000) means the
carveout was not set up (not booted through hekate's L4T entry). A
GR idle wait that times out names its step and prints GR's status,
activity, interrupt, DISPATCH error and trapped method registers; a
self-test kernel with a wrong result logs its first wrong word. A
batch that never finishes prints `[gpu] batch N not done` with the
engine status and the MC, FIFO, PBDMA and GR interrupt registers, and
powers the GPU down too. The SYSTEM page's gpu clock row follows the
same states, and its GPU row shows the share of each second with a
batch in flight: `run bench full 30 &` then `gpu load` should give 97%
or more, and the GPU row the same. The gpu clock row opens the GPU
panel: a step within the cap applies at once, one past it (yellow)
asks first, then goes with `force`; each change shows in `[gpu] GPU
... measured` lines as with `gpu clock`. `./tools/usbterm.py --reload
gpu` replaces the driver in place; the new one finds the GPU on and
powers it down first, and the screen maps the new one's stats page
within 5 s.

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
22. 24/09/2026, Erista, commit `5644725` (touch panel power-up): `input`
    now finds the controller, `[input] touch controller 0x3670, firmware
    0x0100`, and touches arrive (`events`: down, move and up with
    coordinates) but only polled: `touch interrupt stuck low, polling
    instead`. GPIO X1 reads low with no event pending, with the pad as
    found (pull-down, `0x54`) as with a pull-up (`0x58`), no pad in deep
    power-down (`IO_DPD_STATUS` 0), and even with the controller held in
    reset (J7 low): the controller does not drive it. Atmosphère's GPIO
    table names X1 `TpIrq`, yet Horizon's initial pinmux leaves that
    pad's input off (`pinmux_initial_pad_config_icosa.inc`). `stdtest`
    17/17; `ktest` 28/29, `tickless idle cores` failing (13 interrupts
    on core 0 in 300 ms), likely the polling. The RTC reads 2000-01-15.
23. 24/09/2026, Erista, commits `f892d5c` to `b4e698e`, installed over
    USB-C (`--put` of `bl33.bin` and its `bl31.bin`, the old ones kept
    as `.old`): the console marks every log (35 at boot) and `usbterm`
    shows the session alone, the banner and the prompt. With `i2c`
    sleeping once per transfer, `ktest` passes 29/29 four times in a row
    with the touch panel still polled (run 22: 28/29). The SD card was
    mute this boot, as in runs 18 and 19: the card answers (CMD13, state
    4) but its first read times out and the three retries fail with EIO,
    LDO2 at 3.3 V; `fs` finds no volume, so `stdtest` was not run. The
    same commits written to the card from the PC then worked (storage,
    TERMINAL page): the mute card came with the image sent over USB-C.
24. 25/09/2026, Erista, commits `26a6121` to `ed037a8` (`cpufreq`, CPU
    panel): tapping 1122 MHz froze the console at once. PLLP reaches the
    CPU through a branch (CLK_OUT_ENB_Y bit 31, PLLP_OUT_CPU) that Linux
    and Atmosphère's warmboot open before selecting PLLP; with it off
    the cores lost their clock. The switch now opens it, goes through
    CCLKG_BURST_POLICY (0x368) as they do, and checks the CPU really
    slowed down before turning PLLX off.

25. 25/09/2026, Erista, commit `3ca3fea` (no SD card: `fs` exits, as
    expected): CPU speedo 2079, steps up to 1224 MHz. Each change logs
    the clock measured against PLLP, matching PLLX: 1020 -> 1018 MHz at
    1118 mV, 1122 -> 1114 MHz at 1168 mV, 1224 -> 1220 MHz at 1218 mV,
    `boot` -> 998 MHz at 950 mV. `bench burn` scales with the clock:
    31.6 GFLOP/s at 998 MHz (one vector FMA per cycle and core), 32.2,
    35.3, then 38.7 at 1219 MHz for 60 s without a fault, SoC at most
    42.4 C; `bench` at 1219 MHz: `compute_ms` 131 (160), null syscall
    196 ns (240). The fan followed Nyx's steps by itself: 22%, 3180 rpm
    from 42 C. `ktest` gives 28/29 now and then at either clock: the
    tickless test sees 35-57 interrupts on core 1 in 300 ms.
26. 25/09/2026, Erista, commits `1557dcf` to `7cc248a`, the GPU
    firmware sent with `--put` and `gpu` with `--reload`: **the GPU
    runs**. The first `gpu on` stopped at stage 7, `no texture
    processing cluster`: GPC 0's FS_GPC also holds the ZCULL count in
    bits 20:16 (nouveau's u8 drops it, the emulator returned the bare
    count). Masked, power-up takes 37 ms: rail 868 mV (GPU speedo
    2081), GPCPLL at 76.8 MHz, WPR 0xffd00000-0xffd40000, ACR mailbox 0,
    1 GPC, 2 TPC, 2 LTC, context 79872 bytes. `gpu test` then gave 4/5:
    the copy kernel stored what R3 held in the previous launch (the
    fill value, then each earlier copy's data, through any GPU address),
    its LDG setting with stall 1 the barrier the next STG waited on,
    while a barrier arms a cycle after issue. With stall 2: 5/5 in 15
    runs, `gpu bench` 30.4 GFLOP/s (77% of the 39.3 peak), chip 37 C.
    Still open: 1 to 4 power-ups in 10 fail at stage 8, `GR busy in the
    bundles`, a DISPATCH `INJECTED_BUNDLE_ERROR` (0x404000 = 0x80000001,
    exception 0x1, GR engine busy, every unit idle) at a different
    bundle each time; the next `gpu on` works.
27. 25/09/2026, Erista, the GPU clock steps, `power`, `gpu` and `bench`
    over USB-C, the shell as `/bin/sh2`: stage 8 fixed (FE's go-idle
    timeout kept off through the bundles, as nvgpu does: 33 power-ups,
    none failed). `gpu clock` went through every step: measured 153.7,
    537.8, 614.6 (900 mV), 691.4 (943.75 mV), 768.2 (987.5 mV), 845.1
    (1031.25 mV, forced) and 921.9 MHz (1068.75 mV, forced), `gpu test`
    5/5 at each, and back to 76.8 MHz with the rail at 868.75 mV again.
    `gpu bench` scales with the clock: 60.9 GFLOP/s at 153.6 MHz, 335.5
    at 844.8, 365.8 at 921.6, and `gpu sweep` holds 77.4 to 77.5% of
    peak at every step up to the 768 MHz cap. The charger reads power
    good on the PC's USB port (cap 768 MHz); 844 without `force` is
    refused. `bench full` at 998 MHz (CPU) and 768 MHz (GPU): 31.7 +
    304.4 = 336.2 GFLOP/s, SoC at most 49.6 C after 30 s, fan 32%.
    Not exercised: the battery cap (the USB-C console needs the cable)
    and the 80 C fallback (the SoC stayed under 50 C).

28. 16/10/2026, Erista, the debug channel (`usbcon`, `display`, `input`,
    `tools/nxdbg.py`), installed over USB-C with `--reload`: `shot` reads
    the real screen (1280x720 landscape, 0.09 s), taps change the page
    reliably (7 of 7), and the virtual gamepad moves the outline, presses
    A on the TERMINAL tab and turns pages with L and R. The first tap
    after a `display` restart was lost: `input` answered it to the dead
    instance's pending read. It now keeps the event when the reply
    fails; `usbcon` retries a call once when a service was restarted.
    `ktest` 29/29, `stdtest` 17/17 afterwards.

29. 16/10/2026, Erista, `gpu` (`gfx.rs`): **the 3D engine (MAXWELL_B,
    0xB197) draws**. `gpu test` 7/7: the clear writes 0.1/0.2/0.3 as
    0x19/0x33/0x4C (the hardware truncates 25.5 and 76.5), and a triangle
    with a hand-assembled vertex shader (ALD, AST) and fragment shader
    (IPA) interpolates its three vertex colors into a linear render
    target. Two traps on the way, both reported by the batch timeout dump
    (`gr::report`: trapped method and data error code): SET_SPH_VERSION
    is not a method of this class (illegal method), and a shader's
    program offset is refused (INVALID_VALUE) unless the header sits at
    0x30 modulo 0x80, so that the code 0x50 later starts on a 0x80
    boundary, as Kepler and later want.

30. 16/10/2026, Erista, `gpu` 3D: **depth, constant buffers, indexed
    draws and a lit shader**. `gpu test` 9/9: a block-linear render target
    with a 32-bit float depth buffer (near triangle kept over a far one),
    and a quad of two triangles through an index buffer, a vertex shader
    with a model-view-projection and a normal matrix from constant buffer
    1, and a fragment shader with perspective-correct interpolation and
    a directional light: the center pixel is exactly the expected
    0xff0060bf. Findings: the engine refuses a depth buffer with a
    pitch-linear color target (INVALID_OPERATION on the clear); block-linear
    surfaces write nothing unless their PTE carries a storage kind
    (generic 16Bx2, 0xFE, for color; ZF32, 0x7B, for depth); a constant
    buffer's size is taken in multiples of 256 bytes (INVALID_VALUE
    otherwise); INVALIDATE_SHADER_CACHES has no UNIFORM bit and
    INVALIDATE_CONSTANT_BUFFER_CACHE is not a method of this class.

31. 16/10/2026, Erista, games: **Starlane runs on the Switch at 60
    fps** from the library. A game is launched by `display` (spawn,
    wait thread), takes the screen through `SCANOUT` (three 720x1280
    block-linear images, `DC_WINBUF_SURFACE_KIND` block, flipped with
    the window's update and activation request), and draws with the GPU
    at 768 MHz: lit meshes with depth, perspective-correct shading,
    pixel-font text through a GPU canvas. HOME in the middle of a game
    kills it and brings the library back. Three findings: a shader that
    sets a scoreboard barrier needs a stall of 2 before an instruction
    waits on it (a stall of 1 read stale registers: speckled faces); the
    interpolation of perspective attributes takes the reciprocal of the
    interpolated 1/w; and a debug capture longer than a frame must read
    a copy of the image, not the one the GPU is drawing on.

32. 16/10/2026, Erista, the Joy-Con driver (`joycon`), installed over
    USB-C (`--put` of `bl33.bin`, `bl31.bin` and Starlane; `--reload
    joycon` afterwards): the **left Joy-Con works** as a gamepad. It is
    detected on GPIO E6, answers the wake and handshake at 1 Mbaud, then
    its controller info (type 0x01) and the switch to 3 Mbaud, and the
    HID connection; the HID rate reply takes tens of ms, longer than
    hekate's 2 ms wait. Polled every 15 ms it answers 91 to 100% of the
    polls (a latency that drifts between 1.3 and 8 ms against the
    Joy-Con's own 15 ms clock). Two findings: a reply may pause after
    its first bytes (the 250 us gap of `uart_recv` cut it in three: the
    gap is 2 ms until the 24 bytes we parse are in, and a read looks for
    the packet's magic), and the stick rests far from the middle of its
    range (1457, 2393 of 4095) so the centre is measured, not assumed.
    Buttons (D-pad, L, ZL, minus, capture, stick click) and the stick
    (to -32767 at its stop) reach `input` and show on the LOGS page; a
    game started from the library still runs. **The right Joy-Con is not
    detected**: clipped on, its detect line (H6) and the UART-B TX net
    stay high whatever the pad pulls, the charge enables and the rail do
    (a forced handshake gets no byte): not understood yet. `ktest` 29/29
    and `stdtest` 17/17 in the emulator, where `joycon` only logs
    `no handshake`.

33. 16/10/2026, Erista, `gpu` 3D: **textures**. `gpu test` 10/10: a 16 x
    16 block-linear RGBA8 texture (four colored quadrants) on a quad,
    sampled nearest (hard seam) and linear (the seam pixels 0xff006c93
    and 0xff00936c, the blend at the texel centers' distances), through
    the texture header (TIC) and sampler (TSC) pools and `TEX.B`.
    Starlane's ground is a 64 x 64 textured neon grid that scrolls with
    the lane. Findings: the bindless `TEX` takes the coordinates in the
    first source vector and the handle (header index | sampler index <<
    20) in the second one, as Mesa's Gallium gm107 emitter has it; NAK's
    handle-first vector ahead of the coordinates trapped the warp with
    ILLEGAL_INSTR_ENCODING (MP warp error 0x9). `gr::report` now prints
    the GPC/TPC traps (TPC exception 0x2 is the MP; warp and global
    errors) behind a GR exception 0x1000000.

34. 17/10/2026, Erista, **audio**: `audio tone` and Starlane's sound
    effects on the speakers. Path: `nxgfx::Audio` ring -> `audio`'s pump
    thread -> ADMA ring (8 periods of 512 frames) -> ADMAIF1 -> crossbar ->
    I2S1 (master, 1.536 MHz) -> ALC5639 (I2C1 0x1c, MCLK = EXTERN1 12.288
    MHz) -> speakers. Findings, in the order they cost time: (1) the hub's
    first I2S (0x702d1000, the DT's i2s1) takes the CAR clock **I2S0**,
    not I2S1: with the wrong one every I2S soft reset stayed pending and
    nothing left the chip; (2) PLLA_OUT0 came out 1.5 times slower than the
    DT's 36.864 MHz (the ADMA paced at 32 kHz with /24 for I2S): I2S0 /16
    and EXTERN1 /2 give 48 kHz and 12.288 MHz; (3) the codec needs rt5640's
    probe writes (private registers of its `init_list`, GCTL1 0x0301, mic
    bias, DSP path 2) before anything plays; (4) left on without a bit
    clock the speaker amplifier hisses and picks up the CPU's activity, so
    the speakers are muted when the stream is idle; (5) the ADMA's period
    count restarts from zero with the channel, and the ADMAIF wants its
    stop sequence (disable, wait, reset) between streams, else the next
    one runs 16 times too fast; (6) with four 128-frame periods and a 1 ms
    pump the ring was lapped by up to 29 ms while a game ran: thread
    priority 6 and a bigger ring cured it (max one period late). A 1 kHz
    tone and a 440 Hz one were heard; inside Starlane the sound was still
    reported distorted or noisy (TASKS.md 3). The first commit of the audio
    service said the tone was heard before it was: it was not (the I2S
    was stuck in reset, only codec pops reached the speakers).

35. 17/10/2026, Erista, the **battery**: `power` reads the MAX17050 fuel
    gauge (I2C1 0x36) and the BQ24193 charge state, installed with
    `--reload power` / `--reload display`. The status icon is filled to the
    level with its percentage, and Settings has a Battery page. Read on
    the PC-powered console: 5%, 3.508 V, 35.8 C, 4485 mAh full, health 96%
    of the 4667 mAh design, 4.16 cycles. The charger said charging while
    the average current was -440 mA (the PC's port gives less than the
    running console draws; the charge fell 250 -> 246 mAh), so the page
    says "input too weak, still draining" then. Not checked: the charge
    current on the official charger, the temperature against Horizon's.

36. 17/10/2026, Erista, **Bluetooth**: `bluetooth` brings the BCM4356's
    Bluetooth core up on UART-D (new boot image, then `--reload
    bluetooth` / `--reload clk` while iterating) and pairs a Pro
    Controller (9C:10:02:01:63:25, firmware 3.48, likely third-party).
    What the hardware needed beyond Linux's sequence: (1) 115200 baud as
    hekate's `uart_init` sets it, PLLP without the CAR's 16x divider
    mode and divisor 221 (`clk` `BAUD_115200`): with `UART_DIV_EN` the
    divisor latch is not honoured, the reset left at 3 Mbaud and each
    command came back as one `0x00`; (2) BT_WAKE (PH3) low: the patched
    firmware ignores the UART with it high (the ROM answers either way);
    (3) a Remote Name Request before Create Connection: paged straight,
    the controller ended every attempt with 0x10 after ~5 s, the
    connection made right after the name page works; (4) a reconnecting
    controller accepted as slave without our own authentication (both
    sides authenticating left encryption pending until the 20 s setup
    limit); (5) sniff mode at 15 ms after setup: until then the
    controller answers each subcommand and sends no report, then it
    streams full reports at ~60 Hz (1682 in 28 s), D-pad presses seen.
    The pins (BT_RST PH4 high = on, PMIC 32 kHz on GPIO4 already
    running) and the patch file (`BCM4356A3.hcd`, 193 records, LMP
    subversion 0x2409) were right from the start. Pairing then
    reconnection on HOME work. Not checked yet: a genuine Pro
    Controller, Joy-Cons, several controllers at once, a game played
    with it.

37. 18/10/2026, Erista, the **Controllers page**: `input`, `joycon`,
    `bluetooth` and `display` reloaded with the event source word. The
    page lists the left Joy-Con on its rail and the Bluetooth Pro
    Controller; the Test tab shows one card each, and their buttons light
    up and stick dots move when pressed (checked by hand on the screen;
    the debug channel's captures missed the presses). The left stick
    moves through the menus; Starlane was played with the Pro
    Controller.

38. 18/10/2026, Erista, the **shader compiler**: `gpu` reloaded
    (`--reload gpu`), then `gpu test`. 14/14: the classic ten, 16 random
    programs of the IR on 64 threads (divergent `if`, loops with break and
    continue, atomics-free) giving the CPU reference's words, nine draws
    from the built-in pair through the custom path up to varying, uniform,
    texture and discard, four instanced triangles, and R32F, RGBA16F and
    RGBA32F targets rendered then sampled back. Three things the hardware
    refused first: a vertex attribute slot marked inactive needs the
    RGBA32 float format too (data error at `SET_VERTEX_ATTRIBUTE_A`); a
    pixel shader with no input and no position.w declared hangs the
    shader unit (the header now always declares it); and a pitch-linear
    target of 64 bytes per row stops the front end at the first draw (use
    256). A sine of an argument of 2e5 differs from the exact one by
    tens of percent: the fuzzer keeps sine arguments within six.
    The `restart` command only works from the first shell: a `run
    /bin/shell` inside it takes the console input for good, and only a
    reboot gets it back.
39. 18/10/2026, Erista, **GPU memory caching**: the ray kernels took 171 ms
    (closest) and 98 ms (any hit) for 65,536 rays; an empty launch is 19 us
    and the FFMA bench 305 GFLOP/s, so the time was memory. Every GMMU page
    was volatile (L2 bypass); a chain of dependent loads over a 16 KB table
    ran 17 times faster on cached pages (56 ms to 3.2 ms for 65,536
    threads), 3 times faster over 256 KB, and the same over 4 MB (DRAM
    bound, past the 256 KB L2). Client buffers and textures are now mapped
    cached (`Vm::map_at_cached`): rays 53 ms and 35 ms (1.23 and 1.86
    Mrays/s), `gpu test` 15/15, Lumen 60 fps (its ray room 59 fps, was
    about 30). `gpu test` prints `probe` lines (empty launch, FFMA and
    load chains) for tuning.
40. 18/10/2026, Erista, **the ray kernels after a compiler pass**: vector loads
    written in place (no copy out of a scratch quad), box tests of one FMA
    per bound, 36% fewer instructions per ray on the simulator (closest hit
    6555 to 4164). On the console the first build gave 33,480 of 65,536 rays
    wrong: queued loads read their address registers late and the next
    load rewrote them (the simulator reads at issue, so it agreed with the
    reference). Loads, local loads and atomics now signal a read
    scoreboard; rays are right (0 differ), closest hit 45.9 ms, any hit
    28.0 ms (1.42 and 2.33 Mrays/s; 53 and 35 ms before). Time follows
    the loads, not the instruction count: serialised, the cheaper kernel
    took the same 54 ms. Lumen: 60 fps, its ray room a steady 16.7 ms
    frames. A `--reload gpu` with Lumen still on screen (started with
    `run` from the shell, in its ray room) left the console unresponsive
    until a reboot. Separately: such a game is not owned by `display`;
    after HOME the library is back and the GPU is switched off, but the
    process is still alive (`ps`). A reload in that state has not been tried.
41. 18/10/2026, Erista, **coherent rays**: `gpu test` also traces a camera's
    256 x 256 rays (row by row) and the shadow rays toward the sun from their
    hits, against the CPU walk (0 of 65,536 differ each). Soup of 4,000
    triangles: closest hit 4.8 ms for the camera's (13.7 Mrays/s), 2.4 ms for
    the shadows (27 Mrays/s), 42 ms for random rays (1.6 Mrays/s). Most of
    that is the rays being easy (21% and 5% hit): the same rays in a fixed
    random order take 11 ms and 5.7 ms, so warp coherence is worth about
    2.3x. Valley of 41,000 triangles (height field, 1,000 trees; every
    camera ray hits): camera 9.8 ms (6.7 Mrays/s), shadows 4.9 ms (13.5
    Mrays/s), shadows shuffled 10.8 ms (6.0 Mrays/s). Two levers that did
    not pay: the LDG's `.ci` read-only mode (bits 46-47 = 2, envytools'
    gm107 table) makes a 16 KB chase 3x faster but the rays 4%, and page
    tables read through L2 changed nothing; the default mode behaves as
    `.cg` (L2 only). A game launched from the library (A) is killed by HOME
    (`ps` shows no process afterwards), so `--reload gpu` is safe after that.
42. 18/10/2026, Erista, **ray-traced shadows in Lumen**: `gpu test` gained a
    case for the data path (a float target, a compute kernel sampling it
    with TEX, a fragment shader loading the kernel's buffer: passed at the
    first run, 16/16). In the game, a quarter-resolution pre-pass of world
    positions, an 8 x 4 any-hit kernel and a bilateral filter in the lit
    shader: 60 fps, GPU busy 52% before and 82-88% with the shadows (the Lab's
    Overview steps through the parts: pre-pass 1 ms, rays 2.4 ms, mask lookup
    0.8 ms). Three things the first captures showed: the panel's quarter
    turn makes the screen images 720 x 1280, so a 320 x 180 target had
    texels of 2.25 x 7.1 pixels (a 180 x 320 one has 4 x 4); a depth
    tolerance that ignores the slope rejects every neighbouring texel on
    ground seen at a grazing angle and leaves two grey levels, no ramp;
    the filter's temporaries took the lit shader from 55 registers to 67
    (52 once the mask is looked up before the lighting reads its
    uniforms). The console ran out of battery during the session (a USB-C
    port from a PC does not keep up with 768 MHz and the screen): charge it
    from a charger for long sessions.
43. 19/10/2026, Erista, **the shadow mask over time**: two masks of two words
    a texel, history reprojected with the last camera, a sun disc of 1.5
    degrees jittered per texel and frame, half the texels tracing each
    frame: 60 fps with the GPU 76% busy (82-88% with every texel tracing),
    penumbrae on the edges. After the console ran out of battery and was
    started again, Lumen from the library did nothing: the boot image
    carries the services of its build, so its `gpu` refused the new kernel
    (errno 22, the game then left with code 0, no toast): after a reboot,
    `usbterm --reload gpu` first (or update the image, see "Driver updates
    over USB-C"). A game started with `run` from the shell is not owned by
    `display`: HOME gives the screen back and the game keeps running, and
    two games drew over each other; the Lab now leaves after a second of
    refused frames, and the shell has `jobs` and `kill` (not in the boot
    image until it is updated). `lumen lab MODE` starts with the Lab open
    at a shadow mode (the shell sees its panic messages).
    The mask's rows were read mirrored (window y is `ndc.y * h / 2 + h / 2`
    on this viewport, the panel's matrix having turned y down): fine at the
    middle of the screen, wrong toward the sides, which showed as big ellipses
    where the filter rejected every texel and as shadows pointing the wrong
    way; a probe in the mask view (the position texel at the computed place
    against the fragment's world position) found it. The boot image on the
    card was updated with `--put` to temporary names and `mv` (backups
    `bl33.prev`, `bl31.prev`), then the console was restarted: AutoRCM, so
    `hwboot/inject.sh` for hekate and a tap on More Configs, nxk. The battery
    fell from 31% to 15% in an hour on the PC's USB: use a charger.
    The Lab's ray room together with the shadows runs at 30 fps (33 ms
    frames; 59 fps before the shadows): both together exceed a frame
    (the GPU is not the limit, see run 44).
44. 19/10/2026, Erista, **shadows without strays or trails**: walking, the
    player's shadow came out as a checker with a trail of dots, and without
    jitter whole 8 x 4 groups went dark. `gpu test` now runs the shadow
    kernel on the console against the reference machine (80 x 60 texels,
    positions in an RGBA32F texture): 24 of 4800 texels wrong, varying from
    run to run, while the simulator agreed. The cure is a read scoreboard
    on every variable-latency instruction (MUFU and the conversions read
    their sources late too, trap 10): with each case launched 8 times, the
    old emitter got 18 to 103 texels wrong in 15 of 32 launches, the new
    one none in 96, with or without the CRS spill area that QMDs, headers
    and local memory now declare as NVK does (trap 11). 17/17, ray rates
    unchanged. The trail came from the checkerboard being
    the screen's: a point moving an odd number of texels a frame kept the
    same parity and never traced. A texel now also traces when its point did
    not trace last frame (a fourth word per texel), and the figure's boxes
    are tested every frame outside the history. Walking by day and night:
    no trail, soft tree shadows, 58-59 fps, GPU 69-76% busy (as before).
    After the boot image was updated and the console restarted: `gpu test`
    17/17 from the image's own service; Lumen from the library, walked by
    touch (`nxdbg.py down/move/up` on the left half): the shadow follows
    without trail. Walking, the mask's flicker is 2.0-2.6% of texels, against
    1.4-2.5% with neither jitter nor history: edges moving with the camera,
    not noise. The 50 ms frames in the Lab's strip came from its flicker log
    reading both masks back word by word from uncached memory every 120
    frames; it now samples one texel in 16, only for a Lab started from the
    shell: 60.0 fps, max 16.8 ms with the shadows. The ray room's 30 fps is
    not the GPU (47% busy): `RENDER` runs the batch to its end before it
    replies and `FLIP` then waits for the blank, so the CPU's frame and the
    GPU's never overlap; the room's rays, heat map and readback take about
    3 ms of CPU, and `present` 30 ms with the heat map's 5184 rectangles,
    14.5 or 30 ms without. In the valley the CPU's whole frame is 0.2 ms
    (1.1 ms when the flicker log samples): the budget there is the GPU's.
    Walking to the trees, the shadow edges between tiers were jagged and
    speckled: the history came from the nearest texel (the mask view, which
    now paints texels whose history was rejected red, showed rejections at
    silhouettes only). It now comes from the four texels around the point,
    weighted by nearness and distance agreement: soft edges while walking,
    for about 0.7 ms of GPU (72-81% busy in the Lab walk, 69-76% before).
    The figure's boxes were tested from the scene's offset (10-25 cm), wider
    than an arm is from the body; their rays now start 2 cm off, and a box
    that holds the ray's start is skipped (the leg's box takes in the shoe,
    so a point of the leg lies inside it). Seen from four sides, the lit
    faces stay lit. On the PC, 30 walking frames leave 22 of 1200 texels off
    the truth with the four-texel history as with the nearest texel.

45. 19/10/2026, Erista, boot image of `da8aebe` (`--put` of `bl33.bin`
    with its `bl31.bin`): after the reboot `bt status` said no adapter, the
    core answering each HCI Reset at 115200 baud with a Hardware Error
    (`04 10 01 00`) for the whole bring-up (about 3 s after its 50 ms power
    cycle). Minutes later `bt retry`, the same 50 ms cycle, brought it up
    (patch, 3 Mbaud), Wi-Fi staying up. Wi-Fi's bring-up overlapping a BT
    power cycle (`--reload wlan -c "bt retry"`, twice) did not reproduce
    it: time after the cold boot clears it, not the cycle itself.
    `bluetooth` now does what Linux `hci_error_reset` does, spread over
    two minutes: a Hardware Error during the reset ends the speed probe,
    the core stays off and is brought up again after 1, 2, 4, 8, 16 then
    30 s (500 ms off), the service answering meanwhile; one at runtime
    starts the same series instead of leaving the adapter "ready" and
    dead. `bt status` tells the series (`next in N s`, `up at bring-up N,
    S s after the first`): the console has no boot log to replay.
    `nxtest.py --bt --bt-hw-errors 3`: up at bring-up 4, then a pairing.
    On the console, `--reload bluetooth` came up at the first bring-up:
    the normal path still works, the recovery has not run on hardware yet
    (next boot image). Separately, `wlan`, `bluetooth` and `joycon`
    read-modify-write GPIO port H's plain registers from separate
    processes: no lost update showed here (the failure's dump has EN, RST
    and BT_RST right), but the masked registers would make it safe.

46. 19/10/2026, Erista, `--reload gpu` and `display`, games `--put`:
    **the volume indicator and the drawer over a game**. Window C (window
    B scans out black and underflows on this board) blends the UI's
    image, premultiplied, over Lumen: VOL_UP / VOL_DOWN show the pill,
    `+` opens the drawer (brightness, volume, Flight Mode), the D-pad
    moves in it, B closes it; HOME still ends the game. No underflow on
    A or C. The game's `FLIP` were answered by `display`'s loop, which
    also redraws the overlay (5 ms drawing, 10 ms copying to the
    uncached image): 33 to 49 ms gaps, 2 to 4 frames lost in 4 s of
    drawer. Polling bit 1 alone (window A's request) let flips return
    early, 71/s on a 60 Hz panel: the wait stays on bits 0 and 1. With
    `FLIP` and `EVENT` on their own endpoint and thread (priority 8,
    `SCANOUT`'s handle 3, the registers under one lock): 59 to 60 flips/s,
    longest gap 16 ms, no frame late, idle or moving through the drawer.
    Touch over the game: the brightness slider drags, a tap beside the
    drawer closes it. `SNAPSHOT` holds the lock while it copies the game's
    image (one capture showed a block of the next frame's clear color
    otherwise). Window C is not double-buffered: a redraw may show half
    old for one frame where it changes. The boot image's `display` still
    speaks the old protocol: after a reboot, games need `--reload display`.

47. 19/10/2026, Erista, same boot, `--reload display`: **run 46 was
    wrong on the panel**. Its pictures came from `SNAPSHOT`, which blends
    the overlay in software; on the screen, `+` showed nothing and the
    game seemed frozen (the invisible drawer took the input). The output
    CRC (`COM_CRC_CONTROL`, game held by the link lock) was the same with
    window C on or off; white and black patches gave the same CRC: C no
    longer fetched, its registers set like B's and D's. In the boot of
    run 46's first tries B was the one stuck, C fine; a reboot freed B.
    hekate leaves `CMD_REG_ACT_CONTROL` at 0x454: windows A to D take
    their registers at the next line, not the next frame; hekate clears
    D's bit before drawing its log on D. `display` now clears A's, B's and
    D's, checks at start that D (then B) fetches (16x4 patch, two CRCs)
    and draws the overlay on it. Window A's flips now latch at the
    vertical blank: waiting on its own request bit gives 59 flips/s (71/s
    before, flips taken mid-frame, so tearing). Seen on the panel: the
    volume pill and the drawer over Lumen. Underflow counters (0x80A)
    replace `CMD_INT_STATUS`, which read 0. Sleep Mode is in the drawer
    over a game, and a short Power press sleeps there too: the game waits
    in its next `FLIP` (picture held, GPU idle) until the wake; its clock
    jumps by the sleep. Not proven yet: that C stays usable after a reboot
    with the vertical-blank activation (it should, nothing uses C now).

48. 20/10/2026, Erista, same boot (services `--reload`ed), battery 29 %,
    USB-C cable in: **a low-power sleep**. `power` `MEASURE` cuts the
    charger's input (BQ24193 HIZ, its 40 s watchdog kicked every second)
    so the gauge reads the system's own draw, at 1 Hz for 60 s after 15 s
    (`/bin/powertest`); every row is a mean of 60 samples, the cell at
    3.54-3.62 V, Lumen running unless said, usbcon up (the cable's PHY
    and PLLU count in every row), Power to sleep and to wake:

    | state | draw |
    |---|---|
    | library awake, backlight on | 3.11 W |
    | old sleep (backlight off, CPU 612 MHz), game held | 4.18 W, 4.17 W |
    | old sleep, no game (GPU off) | 2.43 W |
    | same, fan off / and `wifi radio off` (WLC_DOWN) | 2.40 W / 2.41 W |
    | no game, GPU on at 76.8 MHz (rail 868 mV) | 2.75 W |
    | sleep 1: CPU and GPU lowest, window A off | 2.61 W, 2.61 W |
    | sleep 2: + services `SUSPEND` | 1.86 W, 1.81 W |

    An idle GPU at 768 MHz (987 mV) cost 1.76 W asleep: no clock gating;
    at 76.8 MHz 0.31 W. Sleep now lowers both clocks, stops window A's
    fetch, holds the game in its next `FLIP` and sends `proto::SUSPEND`
    to `joycon`, `fan`, `input`, `audio`, `bt` and `wlan`: 4.18 W ->
    1.84 W (-56 %). The wake: the picture 38 ms after the press, the
    services 145 ms, Bluetooth and Wi-Fi bring-ups (firmware, patch)
    after; the left Joy-Con handshakes again, the game goes on where it
    stopped (two captures 4 s apart asleep identical, its clock without
    the sleep). The audio codec NAKed its reset 75 ms after its LDO came
    back: the LDO stays on asleep. Touch comes back, but its interrupt
    then reads stuck low (polled). Not done yet: the panel, DSI and DC
    (no panel init of ours yet), the GPU rail (0.31 W, needs its state
    restored), LDO1 (`SUSPEND_RAIL_OFF`, might drop the USB console), the
    CPU cores 1-3 (no CPU_OFF), EMC and DRAM at full speed (SC7 or EMC
    scaling).

49. 20/10/2026, Erista, boot image of a3e3208 plus the panel's sleep
    (`--put` to `/nxk/bl33.new`, the image before kept as `.prev`): init
    grants DSI A to `display`. Asleep the panel gets DCS display off and
    sleep in (host packets inside the video stream, one frame each, as
    hekate's `display_end`), awake sleep out, 180 ms, display on, then
    the backlight: the picture 196 ms after the Power press (38 ms
    before). Same boot, same battery (3.57 V), Lumen held, A/B of the
    two `display` builds: 1.73 W, 1.71 W with the panel on, 1.68 W,
    1.66 W with it asleep: -50 mW. The rest of the display path (DC
    scanning, DSI in HS, PLLD, the panel's rails) needs our own panel
    init to come back from. Also: at boot `display` sent the saved volume
    before `audio` was published, so audio kept its loud default (70 %);
    it now retries for up to 10 s.

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
