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
./fetch.sh        # hekate RCM payload + Nyx SD pack -> dl/ (pinned v6.5.4)
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
  VERSION, FEATURES, CPU_ON, CPU_OFF, AFFINITY_INFO, SYSTEM_RESET; no
  SYSTEM_OFF, which needs the PMIC). CPU_ON follows exosphere: assert
  the core's CPU/CORE reset, ungate its PMC partition (CE1-CE3), release
  the reset; the core restarts at the monitor's `_start` (SB AA64 reset
  vector, set by hekate and rewritten by the monitor: hekate's lock only
  blocks non-secure writes, which is why nxk in EL2 cannot wake cores
  itself), gets its EL3 and banked GIC setup, then enters nxk in EL2.
  CPU_OFF too (`PowerOffCpuImpl`): the flow controller's `CPUn_CSR` arms
  a power-gate of the core at its next WFI, then L1 flushed by set/way,
  SMPEN off, GIC CPU interface muted, `wfi`; CPU_ON first waits for that
  gate, and a woken core clears its flow-controller registers. The deep
  sleep plan built on it: `SC7.md`.
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
on when launching an L4T entry, and the monitor then the kernel draw on
it until the `display` service takes it over. By default that is the
**boot screen** (`bootsplash/`, the same picture at every stage): the
nxk logo and a progress bar on the dark theme's background, landscape
(hold the console normally). Hold **Vol+** while nxk starts to get the
logs as text instead (white on black, portrait like the hekate text
menu: hold the console vertically). In order:

1. hekate logo, then one **line** filling up at the bottom of the
   screen: the hekate countdown (`bootwait`), cleared just before the
   jump to `bl31.bin`. Normal.
2. Boot screen, bar at 15 %: the monitor is running (with Vol+: a
   black screen and `[nxk-bl31] ...` lines).
3. Bar at 30 %, then 45 % when init starts: the kernel is running (with
   Vol+: the `nxk v0.1.0` banner, `[mem]`, `[screen]`, `[boot] ...
   launching init` lines). A kernel panic, a monitor error or a program
   killed on a fault before `display` is up turns the boot screen into
   text: the messages so far, the failure last.
4. The `display` service takes over the screen: the same boot screen,
   its bar going on from 60 % until the users are read from the card
   (15 s at most without one; buttons and taps are dropped meanwhile),
   then the library (HOME), or the first-start setup on a new card:
   user space is alive.
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

Photograph the screen wherever it stops: the bar's length (or with
Vol+, the last line) locates the failure. If hekate's logo stays on
after its line clears (1 without 2), the monitor did not run or did not
find the screen.

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

Right after boot `display` and `input` are not published yet (in
tegra_qemu, about 3 s for `display`, which then takes up to 0.2 s per
request while it draws its first frames; not yet timed on the Erista,
where it adopts hekate's display path): `nxdbg.py` sends a request
again while it is refused with `ENOSYS` or `EPIPE` (nothing was
delivered), and a capture also after `ETIMEDOUT`, up to `--timeout`. A press is never sent twice: a
timed-out `INJECT` may have reached `input`.

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
data: `/users/ID/`, DEV.md "Users and save data"). `mksd-hw.sh` installs the ones built from
`jeux/`; for a card in the console,
`./tools/usbterm.py --put LOCAL:/games/NAME/NAME` (the directory first:
`-c "mkdir /games/NAME"` in an earlier call).

Game catalogs ("services", DEV.md "Services: game catalogs") go the same
way: `mksd-hw.sh` installs `services/*/` in `/services/NAME/`; for a card
in the console, `-c "mkdir /services" -c "mkdir /services/NAME"`, then
`--put` the program and its `service.txt`. L and R on the home screen
switch between Intern (`/games`) and them (run 55).

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

## DRAM clock

`emc` switches the EMC between the rates hekate's Minerva trained
(`minerva_prep_boot_l4t` leaves it at 800 MHz and copies its tables to
DRAM at 0xFFE80000: the SECFW carveout + 512 KiB, MC GSC1 lets the
CCPLEX read it; the kernel hands `init` a read-only window on that range
alone). The sequence is a Rust port of Minerva's `OP_SWITCH` (`emc/`);
the DRAM voltage is never touched. Expected in the boot log:

```text
[emc] at start: MC matches the 800 MHz entry in 24/24 compared registers
[emc] 800 MHz now, tables for [204, 408, 800, 1331, 1600] MHz
```

The arbiter hysteresis group (MC 0xBCC-0xBEC, nine registers) never reads
back as written and is not compared; MC 0x94's bits 16-18 move by themselves.
A few differing MC registers are logged with their values (up to 3 are
tolerated); more, or no entry for the running rate (`[emc] ... no table
entry`), and every SET is refused (ENODEV): the table does not describe
the DRAM that is running. At the shell:

```text
emc               rate, entries (periodic-training ones are refused), switches
emc 204           switch: [emc] 800 -> 204 MHz in NNN us, MC matches after, DRAM MB/s
emc 800           back
emc sleep on      let display's sleep drop to 204 MHz (off by default)
```

First trial, one change at a time, no game running, cable in: (1) the
boot log above (`emc bench` needs a shell newer than the image's); (2) `emc 204`, `emc`, `emc 800`, then a few cycles with
`bench`/`gpu test` between them; (3) sleep and wake with the drop off;
(4) `emc sleep on`, Lumen held, sleep, wake: the log shows `[emc] 800 ->
204 MHz` after the cores went off and `204 -> 800` first at the wake.
A hang during a switch: hold Power 12 s and reboot; nothing persistent is
written. `./tools/usbterm.py --reload emc` replaces the service (not on
a boot image older than the `emc` driver entry: init's table is in the
boot image).

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
gpu suspend     rail off, its state kept; gpu resume: programmed again over it
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

50. 20/10/2026, Erista, two boot images (the first SC7 stage,
    `SC7.md`): **cores 1-3 powered off while asleep** (PSCI CPU_OFF in
    `nxk-bl31`, kernel hotplug, `cores MASK` in the shell). First image
    (85f6c8f): the cores went off, but CPU_ON (PMC toggle, as exosphere)
    never brought them back; `display` and `joycon` stopped too. Second
    image (de0ed8f): CPU_ON tries in turn the flow controller's own
    power-up, a PMC toggle and a full PMC cycle, and records which one
    worked: always the first. Clearing `HALT_CPUn_EVENTS` is enough for
    a core the flow controller gated to power itself back up; the monitor
    saw each core's partition gated (CE1-3 clear in `PWRGATE_STATUS`).
    Five off/on cycles, `display` and `joycon` running throughout, ktest
    0 failed (its hotplug case included), stdtest 17/17. Same boot, Lumen
    held asleep, battery 13-14 % (cell 3.65 V), USB-C cable in, A/B
    within one sleep: cores 1-3 off 1.506 W, all four on 1.561 W, off
    again 1.486 W: **-65 mW** (4 %). The wake: the picture back, the four
    cores back, the game going on where it stopped. CPU_OFF on its own
    saves little; it is the first step towards SC7, which needs cores
    1-3 off anyway.

51. 20/10/2026, Erista, same boot, `--reload gpu` and `display`, battery
    65 %: **the GPU rail off asleep, its state kept** (`SC7.md`'s GPU
    cold restore). `gpu` answers `proto::SUSPEND`: L2 flushed, channel
    off the runlist, GPCPLL, clocks, rail off; `dev` (page tables,
    channel, context, buffers, the clients' mappings, shaders and
    pipelines, firmware files) all stay in DRAM and RAM. `RESUME` runs
    the cold power-up over them (same addresses): rail, clocks, PLL, FB
    and BAR1 registers, ACR boot (PMU), GR init (FECS, GPCCS), a new
    golden image, RAMFC / USERD reset, the class state pushed again. Any
    request that needs the GPU while it is off resumes it ("resumed on
    demand") and it goes off again after 1 s idle while the console
    sleeps. `display` sends it once the game waits in its held `FLIP`
    and resumes it after the panel's sleep out, inside its 180 ms wait.
    Checks: `gpu test` now ends with a suspend, a resume and the 2D,
    compute and 3D tests again on the same device: 17/17, 10/10 after
    (resume 21 ms, the cold start 236 ms with the card's firmware
    read); Lumen through six sleeps with the GPU suspended, rendering
    right after the wake (a shot before and one after: the same scene,
    the time of day moved on), 60 fps, GPU back at 768 MHz 57 ms after
    the wake: the picture still 223 ms after the press. A/B in one boot, Lumen held,
    cell 3.85 V, USB-C cable in, `powertest start 60 15`, Power pressed
    at once (cores 1-3 off in all):

    | sleep | draw |
    |---|---|
    | GPU at 76.8 MHz, rail on (run 48's) | 1.646 W |
    | GPU suspended, rail 0 mV | 1.327 W |
    | same, `display` reloaded | 1.311 W |

    **-0.32 W (-20 %)**, more than the 0.31 W run 48 estimated for the
    rail. (The `display` measured also called the `emc` service's sleep
    hook, which was not running: a no-op.) The emulator cannot test it (its GPU model runs the first
    power-up only: a second one, with or without this change, times out
    at the first batch); the console did. Not done: SC7 itself (the
    GPU comes back from reset the same way), the late request that
    finds the GPU off while a batch is half-built.

52. 20/10/2026, Erista, boot image of `17efa56` (`--put` to `/nxk/bl33.new`
    and `bl31.new`, then `mv`; the pair before kept as `bl33.pre-emc`,
    `bl31.pre-emc`; the new `bl31` is the one with the SC7 stage 2 warm
    entry, which was not exercised): the **`emc` service**, a Rust port of
    Minerva's switch run from EL0 on the tables hekate left at 0xFFE80000.
    Boot: 5 entries (204, 408, 800, 1331 and 1600 MHz, the last two need
    periodic training and are refused), running at 800 MHz, 23-24 of the 24
    compared MC registers equal to the 800 MHz entry (the nine arbiter
    hysteresis registers never read back as written; MC 0x94's bits 16-18
    move by themselves). 11 awake switches among 204 / 408 / 800 MHz with
    `stdtest` 17/17 between each: 72-74 us to 204, 95-97 us to 408, 104-113
    us to 800, the MC matching the new entry afterwards, and a DRAM copy of
    878 MB/s at 204 MHz, 1489 at 408, 2212 at 800 (the clock really moves).
    A/B asleep in one sleep, Lumen held, `powertest start 60 15`, `emc` run
    from the shell between the measurements (cores 1-3 off, GPU suspended):

    | DRAM | draw |
    |---|---|
    | 800 MHz | 1.305 W |
    | 204 MHz | 1.125 W |
    | 800 MHz | 1.277 W |
    | 204 MHz | 1.102 W |

    **-0.18 W (-14 %)**. With `emc sleep on`, a sleep and wake through the
    Power button: `display` dropped the DRAM to 204 MHz after the cores went
    off (the status showed "asleep: restores 800 MHz", 1.103 W measured) and
    the wake put it back to 800 MHz first, Lumen going on as before. Off by
    default; `emc sleep on` after each boot. The path SC7 needs after the
    boot ROM's 204 MHz is the same call (`emc 800`).

53. 21/10/2026, Erista (InnoLux P062CCA-AZ1, panel id 0x0F20), boot image
    of `86d4cb0` (init grants CAR, GPIO, pinmux, MIPI CAL and APB_MISC
    read-only to `display`; `--put` to `/nxk/bl33.new`, the pair before
    kept as `.pre-display`): **`display` powers the whole display path off
    asleep and brings it back from reset**. Boot: the loader's path is
    adopted (clocks, PLLD locked, controller in continuous display, DSI
    video: 7 checks), and the 57 watched PLLD / DC / DSI / MIPI CAL
    registers read exactly what the bring-up tables write. Asleep: window A
    and the overlay off, display off, controller stopped, PLLD back to
    low power, the panel's deinit (none for this panel), sleep in, reset
    low, AVDD channels off, WLED off, DSI, controller and MIPI CAL held in
    reset with their clocks off, LDO0 off, PLLD stopped (IDDQ): 103 ms. The
    wake: phase 1 87 ms (rails, clocks, PLLD at 100 MHz, controller and
    DSI init, reset, id, sleep out), the CPU clock and the GPU resume
    (56 ms) inside the panel's 180 ms, phase 2 171 ms (display on, PLLD
    468 MHz, video, calibration, the windows replayed): the picture 443 ms
    after the Power press (223 ms with the DCS-only sleep), the services
    560 ms. After each wake 0 of 57 watched registers differ from the
    loader's, and the output CRC of the held Lumen frame is the same
    before and after (three sleeps: 0xdd2adb23, 0xa267bfb8, 0xa6f5c5c3,
    0x35fd994d), Lumen back at 60 fps. The first try read the panel id
    in command mode at the power-down without the device takeover timing
    (`DSI_BTA_TIMING`) and got nothing; with it the id reads (0xF20) at
    the first power-down, which JDI and AUO panels need for their deinit.
    A/B in one boot (`powertest start 60 15` asleep, Lumen held, cell
    3.98-3.99 V, cable in, cores 1-3 off, GPU suspended, `emc sleep` off
    throughout), `display` reloaded between the legs:

    | `display` | draw asleep |
    |---|---|
    | new, path off | 1.126 W, 1.116 W |
    | old (DCS-only sleep, `6e1f73c`) | 1.259 W |
    | new, again | 1.107 W |

    **-0.14 W (-11 %)** over run 49's panel-only sleep. Seen on the panel
    after a native wake: the picture right, and the drawer over Lumen (the
    overlay window still fetches after the controller's reset). The emulator, which has no DSI or panel model, now
    cold-starts the display path at every boot (the screen is 720x1280
    drawn turned, no longer 1280x720); a stopped path found at start
    (a wake that failed, a loader that left no picture) is brought up from
    scratch by `--reload display` too.

54. 21/10/2026, Erista, boot images of `fd48a51` then `edebc38` (`.pre-sc7s2`
    and `.pre-gicwake` kept): **SC7 stage 2, core 0 powered down and back**
    (`coresleep MS` in the shell, PSCI CPU_SUSPEND in `nxk-bl31`, `SC7.md`).
    Core 0 is really gated: the warm entry reports a power-up from the gate,
    not a `wfi` that returned. An interrupt powers it back up through the
    flow controller, `_start_warm` and the kernel's `kresume` rebuild
    EL2/EL1 and the GIC CPU interface, and the system goes on: ktest 0
    failed, stdtest 17/17 after six suspends. It never stayed down for its
    timer: back after 133-139 us every time, with the device lines moved off
    core 0, and with the legacy interrupt controller's wake (it sees every SoC
    line) removed from `HALT_CPUn_EVENTS`. The emulator shows a reschedule
    SGI pending at the power-down (a thread homed on core 0 made ready by
    another core), which a woken core must take; the next image logs the
    pending state and first interrupt on silicon. Also read live (`soc`):
    the BPMP is halted under nxk (`HALT_COP_EVENTS` 0x50000000: hekate's
    `bpmpfw` runs nothing), and the cores' MISSED timer column is non-zero
    on silicon (60/19/6/1 over a million interrupts), likely the race between
    the last interrupt acknowledge and the deadline check.

55. 22/10/2026, Erista, boot image of run 54 with `display` of `d8fd421`
    reloaded over USB-C (`usbterm.py --reload display`) and `services/demo`
    put in `/services/demo/` (`mkdir` first): **game catalogs (services)
    on silicon**. The home screen shows the Intern / Demo tabs; R switches
    and starts the catalog, its list comes, then its covers 1.5 s later; A
    shows the launch card (cover, queue place, seconds), then the game
    takes the screen (its color, from CPU writes to `SCANOUT`'s images); HOME
    sends `STOP`: "Closing Basalt", the list back. B during a launch
    ("launch cancelled"), Offline ("no machine is free"), Crash (the
    catalog exits with code 3, "Demo stopped (code 3)", restarted) all as
    in the emulator; L goes back to Intern and ends the catalog (no `demo`
    process left). Seen with `nxdbg.py shot` (display's copy of the image),
    not yet confirmed by eye on the panel.

56. 23/10/2026, Erista, boot image of run 54, `power` and `display` of
    `9142d46` reloaded over USB-C, `services/luna` in `/services/luna/`:
    **Amazon Luna signed in on silicon**. No Wi-Fi joined, so the PC
    shared its connection over the cable ("Internet over USB-C" above,
    `nmcli ... nxk-share`). `power`'s SNTP set the clock from 2000-01-07 to
    the real time (`RTC offset 845468256 s`, kept in `/config/rtc_offset`).
    The Luna tab showed the QR code and a code; the user entered it at
    amazon.fr/code, the device registered, the player profile was taken
    and the account's playable games listed (Wobbly Life, Avatar: Frontiers
    of Pandora, Goat Simulator 3, Shadow of the Tomb Raider...), their box
    art on its blurred backdrop after a restart of the catalog (none came
    in the first minute after the sign-in: not explained). A later `luna`
    build (one more log line; fine in the emulator) died at once, four
    times, on "undefined instruction" at the start of code pages
    (0xac000, 0xdd000, 0xde000): pages read as zeros. Copying it over the
    running file, or as a new file, made no difference, while the first
    build ran again. Meanwhile the SD card answered EIO before the reboot,
    and the battery read 6, 3, 2 then 1 % **while plugged in**: under nxk
    it drains rather than charges. Tests stopped there: charge under
    Horizon or hekate first, then load that build again to tell a loader
    or SD fault from the low battery.

57. 23/10/2026, Erista (battery 8 %, a short test), boot image of run 54,
    `audio` of the shallow-queue change reloaded over USB-C and the new
    `audiotest` put in `/bin`: the shallow DMA queue on silicon. Speaker
    volume 10 %, level 10 (the console had been left at 0 %). `audiotest
    512 20000 10` twice (a client ring asking for 512 frames: 4 periods
    of 128, about 8 to 11 ms before the speakers), with `gpu on` and two
    `gpu bench` (30.5 GFLOP/s) running meanwhile: 12629 periods played
    over both runs, **0 underruns, 0 late looks, 0 laps, max step 2,
    fewest periods left at a look 2 of 4, longest gap between two looks
    3.5 ms** (a period lasts 2.67 ms). So a depth of 4 has a margin of
    two periods under a busy GPU; depth 3 would have had one. Not yet
    heard or measured: the real latency of Luna's sound (the user's ears)
    and the display redrawing at the same time (the library was on screen).
58. 23/10/2026, Erista (gauge at 7 %, plugged), boot image of run 54,
    `power`, `display` and `audio` of `93466f6` reloaded, Luna of the same
    build in `/services/luna/`: **Luna's sound on silicon** (Wobbly Life,
    speakers at 10 %, about 105 s, the network shared by the PC over the
    cable). nxopus decodes a 20 ms frame in 270 us on average, 0.8 ms at
    most (about 1.4 % of a core); the stream asked the service for 4
    periods of 128 frames. Of ~4600 packets, 332 were concealed (289 came
    after their deadline) and the ring ran dry 140 times: 85 losses in the
    first 2.6 s (the buffer at 23 ms, the drift loop at its -2000 ppm
    limit), the rest in bursts where packets stalled 0.3 to 0.5 s (arrival
    p99 333 to 508 ms) between calm stretches (p99 21 to 34 ms). Not known
    yet: whether the stalls are the network, SPP holding a channel for a
    retransmission, or the console. Before that, the first start of the
    catalog died: `undefined instruction` at `+0xd8560` on a TLS worker
    thread (`num_bigint_dig`), a valid `str x30` in the file: the CPU ran
    other bytes than the file's on a code page loaded on demand, like the
    zero pages of run 56; display's restart of the catalog ran. Covers:
    `HTTP 400` from the image server (none shown).
59. 23/10/2026, same console and image as run 58 (gauge 5-7 %, plugged),
    a second Luna session with the run 58 build: again 37 packets
    concealed in the first 3 s (the buffer at 26 ms: only two packets were
    in hand when play started) and stalls of 0.2 to 0.5 s after. The PC's
    own link was the bottleneck then: its Wi-Fi uplink, loaded by the
    PC's downloads (1.1 MB/s), answered pings in 632 ms on average (3.3 s
    at most, 4 % lost), and an emulator session over it at the same time
    saw a third of the audio packets lost on the network while luna's
    stream thread was never busy more than 5 ms. An instrumented build
    then died at once on the code-page fault of run 58 (`+0xd5170`, a
    page-aligned address, right after "SPP connected"). Reloading
    `display` leaves the old catalog process running (two orphans).
81. 30/10/2026, same console (gauge 15 %, charging), `dev` at `195ec2f`,
    `nxk-ipl.bin` injected, SD card from `mksd-hw.sh`, a game cartridge in the
    slot: **the game card reader's ASIC answers**. `gc probe` (new `gc`
    service): the detect switch (GPIO S3) reads "in"; LDO3 was off before
    (cfg 0x2e, cfg2 0xd2, fps 0xc7), the probe raised it to 3.1 V; reset
    (GPIO BB3) released after 20 ms; SDMMC2 up at 8 bits, 1.8 V, HS100 from
    PLLC4_OUT2; hardware tuning on CMD21 found a tap (the ASIC's bootrom
    answers it); CMD13 answered `0x804` (state 4, ready for data); 46 ms in
    all, then the ASIC and its clocks off again. Nothing past tuning and
    CMD13 was sent: CMD60 loads signed firmware, which burns OTP fuses, and
    reading the card needs the console's keys. Not yet checked: the
    detect switch's polarity with the cartridge out, the Tools page on the
    panel.
80. 30/10/2026, same console, rebooted under nxk (gauge 18 %, charging): `x86test` with
    x87 (80-bit soft-float, register stack, control and status words) sent with
    `usbterm.py --put`: 8/8. The `long double` guest (series, conversions, truncation
    through the control word, 24/53/64-bit precision, fprem loops, overflow, NaN) prints
    the PC's native output bit for bit: 15089 instructions, 3.8 MIPS at 998 MHz, 4.7 at
    1224 MHz. The other guests: 3.2 to 4.8 MIPS at 998 MHz (floating point 3.2), the ALU
    loop 7.3 (8.9 at 1224), a little below run 79's 9.0 and 5.1 to 5.5 for the same
    code: not investigated (the larger binary, the console's temperature or the low
    battery are candidates). stdtest 19/19.
79. 29/10/2026, same console and boot as run 76 (gauge 40 %, charging), `x86test`
    with SSE/AVX floating point on the soft-float core, sent with `usbterm.py --put`:
    7/7. The float guest (float and double arithmetic, conversions, specials, FTZ and
    DAZ, MXCSR rounding) prints the PC's native output bit for bit, built generic and
    for Jaguar; the figures shown are the Jaguar build's (63321 instructions, 3.6 MIPS
    at 998 MHz, 4.4 at 1224 MHz), the generic one was only checked for output. The ALU
    loop is unchanged (9.0 MIPS at 998 MHz, 11.0 at 1224), the integer guests 5.1 to
    5.5 (6.2 to 6.7). stdtest 19/19.
78. 29/10/2026, same console and boot as run 76 (gauge 44 %, charging), `x86test`
    with VEX (AVX1), BMI1, MOVBE, CRC32 and SSE3..SSE4.1 sent with `usbterm.py --put`:
    6/6 in both builds below. The third guest pair, built at -O3 for the PS4's CPU
    (`-march=btver2`), prints exactly what the PC's native run prints (456409
    instructions). First build: ALU loop 6.6 MIPS, guests 4.1 / 4.1 / 4.4 at 998 MHz;
    7.9 and 5.1 / 5.1 / 5.4 at 1224 MHz; stdtest 19/19. Second build, the rare paths
    kept out of the dispatch loop (`#[inline(never)]`): 9.0 MIPS and 5.1 / 5.1 / 5.5 at
    998 MHz.
77. 29/10/2026, same console and boot as run 76 (gauge 47 %, charging), `x86test`
    rebuilt with the wider x86-64 decoder and the PS4 runtime (`ps4rt`) and sent
    with `usbterm.py --put`: 5/5. The PC-built guests (`ps4/guest/`, a libc module
    and a main program, gcc -O2) load, link by NID (9 imports to the libc module, 2
    to HLE) and print exactly what the PC's native run prints, in the standard ELF
    layout and repacked in the PS4's SCE layout: 486412 guest instructions, 5.1 MIPS
    at 998 MHz, 6.2 at 1224 MHz (`cpufreq 1224`, back to `boot` after). The ALU loop
    runs at 8.2 MIPS (10.1 at 1224), from 10.6 in run 76: the price of the decoder.
76. 29/10/2026, Erista, card of run 75 (no `abi/`, `std/`, `libnx` or kernel
    change since), `dev` at `7acc0f3`, `nxk-ipl.bin` injected: **the x86-64
    interpreter of the PS4 layer runs on the console**. `x86test` sent with
    `usbterm.py --put`: 3/3, the 1568 ALU vectors identical to a PC's CPU,
    self-modifying code re-decoded, **10.6 MIPS** (3 M instructions in 283 ms,
    CPU at its 998 MHz boot clock: about 94 cycles per guest instruction).
    stdtest 19/19. The battery gauge was not read (no shell command for it).
75. 29/10/2026, Erista, branch `feat/boot-screen` at `5c6c569`: **nxk boots
    without hekate**. `nxk-ipl.bin` injected (fusee-nano), SD card from
    `mksd-hw.sh`. Two failures first, read off nxk-ipl's own screen log
    (Vol+): the 4-bit SD bus times out on this slot (as for the `sd`
    service) and the 1-bit fallback reads; then `nxk/bl31.bin` was "not
    found": Linux had stored it as the 8.3 alias `BL31~1.BIN` with a long
    name, which nxk-ipl's FAT32 reader now matches. Then the shell over
    USB-C: `emc` shows 800 MHz and 5 trained rates (204, 408, 800, 1331,
    1600, Minerva's training done by nxk-ipl), switches 800 -> 408 -> 800
    MHz in 96-99 us; stdtest 19/19; ktest 31/32 then 32/32 twice (the
    core hotplug case failed once: "spinner 0 stopped once cores 2-3 went
    off", three spinners at one priority on two cores).
74. 25/10/2026, same console (gauge 45 %), `luna` at 45 Hz, volume 10 %, the
    capture armed, `init`'s supervision in place (the 30th Luna session, 16
    minutes of Shadow of the Tomb Raider): the stream was steady for the whole
    time (41855 pictures received, decoded in 14.0 ms, 1 dropped, 158 skipped to
    a key frame, 98.6 % shown; FEC: 1596 parity packets, 1483 complete groups
    matched, 6 packets rebuilt and used, 5 compared identical; audio 46740 packets,
    28 lost, 4 underruns). Then `wlan` took a second SError, at another place
    (`pc +0x535c`, the first was `+0x5610`: both in the chip's interrupt path), and
    `init` started it again at once (`[init] wlan exited (code 139): starting it
    again`). The same freeze followed as in run 72: `luna`'s presenter was blocked 42.1 s in
    `show` (the maximum of its report: 42122 ms), `luna` ended with `nothing from the
    server for 42 s`, the new `wlan`'s start-up messages came only when the player
    left, and `net` had gone to `usb0`. 42 s both times. The root port's completion
    timeout is not the cause: its register reads 0 (range A, 50 us to 50 ms) and a
    read of a missing chip returns after 47.8 ms. A hang that stops the other services
    too (display, sound, the other drivers) fits a core stuck on a device access
    while the others wait for it (TLB or cache maintenance broadcast, the kernel's big
    lock); not shown. Open: what the chip's interrupt path touches at that moment, and
    why the access takes 42 s to end in an SError.
73. 25/10/2026, same console, rebooted with run 72's image (`init` of `63b8ddc`):
    **`wlan` comes back by itself**. `wifi poke die` made the driver exit (code
    139): `init` started it again from `/bin/wlan` within a few seconds, `net` went to
    `usb0`, `display`'s first join found the old handle (EPIPE) and tried again 3 s
    later (it waited 30 s before), the network `nxk-test` was joined and `net` was
    back on `wlan0` (10.101.173.75) inside 30 s. The volume is back to 100 % at every
    boot: set it before any sound. `/bin/wifi` and `/bin/wlan` on the card are the
    builds with `poke die` (a `wlan` restarted by `init` from an older `/bin/wlan`
    has no `die`).
72. 25/10/2026, same console (gauge 71 %, plugged), the phone's hotspot, `luna`
    at 45 Hz, volume 10 %, a continuous capture of the console's log: **wlan's
    death caught** (the 29th Luna session, about 7 minutes of Shadow of the Tomb
    Raider). Stream healthy until then (FEC: 739 parity packets, 591 complete
    groups matched, 3 packets rebuilt and 3 retransmitted copies compared, all
    identical). The kernel logged `'wlan' ... system error (SError) at 0x0,
    pc +0x5610 -> process killed`: the code there is the chip's mailbox interrupt
    status read-and-clear after a window write, the load being the instruction
    the error is attributed to. `net` moved to `usb0` (the PC's public address, so
    the server fell silent), the picture froze, and the user heard a harsh tone
    until HOME; `luna` ended 42 s after the last datagram and "the session could not
    be ended now" (the network was on the other address). The tone is not explained
    (the audio service silences the ring behind the filled periods only below
    its full depth; in the games' mode a pump held up more than 85 ms replays the
    ring). `wifi poke` (new, development) takes the module down and times reads of
    BAR0: the first read of the vanished chip takes 47.8 ms (a completion timeout)
    and returns all ones, the next ones at once; with the memory decode off (as
    after a reset behind our back) reads return ones in 4 us, three posted writes
    return at once and no SError follows: a read of a missing chip does not
    produce the SError. 9 sleep and wake cycles, 12 clock switches during pings and
    9 speed tests (23 to 38 Mb/s over 2 to 3 connections, none with a zero after the
    first sample) did not kill `wlan` either; cause still open. `init` supervises
    `wlan` now (starts it again from `/bin/wlan` or the boot image when it exits,
    tried in the emulator with `wifi poke die`), which needs the boot image on the
    card to be rebuilt: done afterwards over USB-C (`bl33.bin` 4317184 bytes and its
    `bl31.bin` 14384 bytes, built from `63b8ddc`, `--put` to `.new` names with the
    CRC checked then `rm` and `mv`; the pair before kept as `bl33.pre-supervise` and
    `bl31.pre-supervise`; `build/nxk.bin` must be refreshed before `mkbl31.py`, which
    bakes the kernel's size in; `test-bl31.sh` OK). Not rebooted yet.
71. 25/10/2026, same console (gauge 17 % then lower, plugged), the phone's
    hotspot, `luna` of `a4170a8`, `refresh=45`, volume 10 % (`wlan` found dead
    again before, restarted with `display` and `net`): **45 Hz holds** (the 28th
    Luna session, about 7 minutes of Shadow of the Tomb Raider). The server
    honours 45 Hz (45 pictures a second received). Over 19556 pictures: decoding
    13.7 ms of the 22 between pictures (34 at most, 3 key frames then 7), 3
    dropped, 35 skipped to a key frame (5421 at 60 Hz), 19379 shown (99 %),
    gamepad to socket 1.0 ms, the delay from a complete frame to its decoded
    picture 9 to 37 ms with a few peaks at 87 ms after a loss (queue 8), the
    rate 3.3 Mbit/s on average.
70. 25/10/2026, same console (gauge 27 %, plugged), the phone's hotspot
    (nxk-test, 5 GHz channel 155, -38 dBm, pings 103 ms on average, 0 % lost),
    `luna` of `2657559` with `notify_resolution=1280x720`, 60 Hz, volume 10 %:
    **the server ignores the resolution notification** (the 27th Luna session,
    about 7 minutes of Shadow of the Tomb Raider). Stream and capture stay 1920x1080
    after the message (sent with the first picture). The server sends about two
    `probe` messages a picture, 1399-1400 bytes of payload each and a next size of
    0: path probing of the packet size, not a bandwidth test. At 6 Mbit/s the decoder
    takes 17.8 ms a picture (34 at most): over 26712 pictures 312 were dropped, 5421
    skipped to a key frame (614 asked), 20776 shown (47 a second), the delay
    from a complete frame to its decoded picture about 100 ms. The 60 Hz does not
    hold on this game. Before it, on the Freebox (-67 dBm) 20 pings gave 15 % lost with
    timeouts in a row, the cause of the speed tests' zeros: the console had stayed on
    `usb0` after the boot (`net` skipped `wlan0` for no DHCP answer; `restart net`
    brought it back) and `wifi status` showed 39610 late items.
69. 25/10/2026, same console (gauge 36 %, plugged), `luna` of `6cf0ba7`, phone
    on Wi-Fi throughout, volume 10 %: **the parity's length** (the 26th Luna
    session, ended after under a minute: the log slices missed why, the Wi-Fi
    icon was grey afterwards). On the display channel 14 groups matched and the
    15th (complete, no copy in it) matched on the header and on every payload
    byte of the parity, which is only 1080 bytes for a group whose packets are
    up to 1384: the parity is as long as the group's last packet and covers that
    prefix only (`spp_send_buffer_push` sets the length at the last packet).
    The same cause as the mismatch of run 68 (1280 bytes). The check compares the
    covered prefix now, and a lost packet longer than the parity is counted as
    too long to rebuild (it cannot be). Audio: 13 groups verified, no loss;
    hole to retransmission 63 ms (70 at most); 342 pictures decoded in 7.7 ms,
    none dropped, delay from complete to decoded 8.5 ms.
68. 25/10/2026, same console (gauge 39 %, plugged), `luna` of `48c4c13`, 60 Hz,
    volume 10 %: **the parity packets, the first time on the real stream**
    (the 25th Luna session, ended by HOME after about two minutes). The parity
    layout of the decompiled client is right: on the audio channel 20 complete
    groups matched (payload and the 8 header bytes) and 3 lost packets were
    rebuilt, acknowledged and fed to the player (2 compared with the
    retransmitted copy: identical); on the display channel 24 groups matched,
    then one complete group (the 25th, in the first second, with a 422 ms gap
    in the datagrams) matched on all 8 header bytes but not on the payload and
    switched that channel's recovery off. Lags from a hole noticed to the
    parity 54 ms (100 at most), to the retransmission 62 ms (151 at most): the
    parity wins only about as often as it loses. The decoder skipped 81 pictures
    to the next key frame over 9 losses given up (0 decoder errors, 11 key frames
    asked, delay from complete frame to decoded 23 ms, 77 at most). Then the
    stream stopped: no datagram for minutes, the picture frozen on Tomb
    Raider's menu, the cloud session ended by HOME. The user's phone, the
    console's access point, had moved from Wi-Fi to 5G during the test: the
    public address changed and the server, which answers one address, went
    silent (`wifi status` counts 2 chip losses since the boot, not shown to be
    in this run). `luna` ends a stream that heard nothing for 15 s now
    ("connection lost", the session ended). The payload mismatch is probably a
    retransmitted copy that differs from the original; groups with a packet that
    came behind a later one are left alone now, and a mismatch dumps the offsets
    that differ.
67. 25/10/2026, same console (gauge 48 to 39 %, plugged), `luna` of
    `d980315`, 60 Hz announced, volume 10 %: **Shadow of the Tomb Raider for
    13 minutes** (the 24th Luna session). The PC's link was good (pings 8 to
    14 ms). The stream came at 60 pictures a second, 98 % of them shown;
    decoding 10.7 to 12 ms a picture (35 at most) at 3 Mbit/s; the gamepad
    reached the socket in 0.7 to 1 ms (2.2 at most). About 1.5 holds a second
    (lost packets recovered by the server's retransmission, 56 to 62 ms after
    the loss): the pictures behind them came in bursts, the queue reached 5 to
    6 and the delay from a complete frame to its decoded picture went from 8 ms
    to 58, then 88 and 127 ms after a longer stall (137 ms without a datagram,
    audio p95 120 ms): the queue overflowed (21 dropped, 100 decoder errors,
    13 key frames asked, the picture frozen). The decoder's 5 ms of margin a
    picture drains a burst slowly. Nothing was rebuilt: the parity packets
    were still ignored. Next: the parity (shadowed until it matches) and
    skipping to the next key frame after a gap.
66. 25/10/2026, same console (gauge 49 %, plugged), `display` and `luna`
    replaced in place (the old `luna` ended by the Intern tab first): **the
    decoder's real margin at 60 Hz, and a second thread for the game's
    `EVENT`**. `h264bench` at 1224 MHz on four threads, ms a picture
    (median / 95th / 99th / max): 1200 pictures of the real 60 Hz stream
    (Avatar, 1.7 Mbit/s) 9.8 / 11.9 / 16 / 37; a fast 1080p x264 clip in the
    server's shape (P frames, 16 refs, 4 slices, 400 pictures) at 8 Mbit/s
    16.5 / 20.5 / 26 / 43, and the 25 KB-a-picture clip 17.5 / 27 / 31 / 44.
    Its profile on one thread (55 ms a picture): motion compensation 17 ms,
    residual parse 7.5, reconstruction 5, the filter 16; the cost hardly
    follows the bit rate (the PC: 7.4 / 8.9 / 9.0 ms at 4 / 6 / 8 Mbit/s). So
    60 Hz is within reach on calm scenes and at the limit on fast ones.
    `display` serves the game's endpoint on two threads: Starlane from the
    library answered LEFT held (the ship moved) at 60 fps, HOME ended it
    (`ps`: no game left). No Luna session was opened.
65. 25/10/2026, same console, rebooted with run 64's image, `display` reloaded:
    **the catalogs' games are cached on the card**. A Luna tab used to open on
    "Starting" and show its covers 1.55 s after the tap; with
    `/users/u1/cache/luna/` (list.bin and 87 covers of 131 KB) it shows the
    games on the first capture after the tap, the program starting behind.
    With Intern shown, 6 s after the boot Luna runs once (up in 1.3 s, 87
    games, 87 covers read off its own card) and ends: 88 files. A first
    version pruned the covers when Luna's first list came empty; empty lists
    are now ignored. TCP buffers of 2 MiB: one connection 13.7 to 34 Mb/s, four
    44 (59-64 best second) with the CPU at 26 % and the hotspot on 2.4 GHz
    channel 6 (the phone does 60 MB/s to OVH on its own link). A scan from the
    Networks sheet while connected made `wlan` fail (EIO, "try Retry"), and
    once `wlan` was killed by an SError at 0x0 (a bus error from the chip) after
    several speed tests, `wifi retry` or `--reload wlan` brought it back.
    The scan failure was the 3 s limit of the scan, not the chip: an escan
    takes 2.7 s with the firmware's defaults and 5 s with 200 ms a channel,
    and while it ran `net`'s carrier probe timed out and moved the console to
    `usb0`; the limit is 12 s, a busy driver keeps its carrier, and with 200
    ms of listening the scan hears the Freebox on 5 GHz channel 36 (-80 dBm;
    the firmware offers 5 GHz channels 36-165 for country US, none was ever
    heard with the defaults). Scanned while joined it finds fewer of the
    weak ones (2 to 6 of 6). Rate on 5 GHz (channel 155, -47 dBm, the phone's
    hotspot): 33 to 44 Mb/s over four connections, the same as on 2.4 GHz, so
    the radio is not the limit. The stack is not either: plain TCP from the PC
    over usb0 (`speedtest -u`, 1 ms) gives 195 Mb/s with one connection or
    four, and the CPU stays under a third of a core in `wlan`. Pings to the
    hotspot alternate 3-5 ms and 50-170 ms, with the power saving at 0 after
    the join and no frame dropped in `wlan` or `net`: the delay is in the
    air or the phone's access point (the cause is not found); a PC on the
    same hotspot would tell.

64. 25/10/2026, same console (gauge 88-97 %, plugged), `display` and `net`
    reloaded, `speedtest` put in `/bin`: **Wi-Fi first and the speed test**.
    Switching tabs: the screen was frozen 2.3 s while `luna` read its 87
    covers (each version change of its state redrew the 27 ms frame, about 74
    times); an update that shows nothing new no longer redraws: the tab shows
    0.06 s after the tap, the covers 1.55 s (3.8 s before), no screen capture
    slower than 0.2 s; `luna` takes 1.3 s to say hello (0.7 s in the emulator).
    Network: `net` now takes `wlan0` first (nxk-test, channel 6, -30 dBm):
    the page reads `via wlan0`, 10.101.173.75, and the Wired chip moves it to
    `usb0` (10.42.0.190, the PC's share) and back within 4 s, `nxdbg` surviving
    both switches. `speedtest` to `proof.ovh.net/files/1Gb.dat` over Wi-Fi,
    8 s: one connection 13.7 Mb/s, four 18.8 to 36.4 Mb/s (best second 41.6;
    the PC, on another Wi-Fi network, ran 23 Mb/s to OVH at the time), connect
    15-212 ms, TLS 34-68 ms. Whether one connection's 13.7 Mb/s is the link or
    nxk (a 64 KiB TCP buffer, 3 KiB per `RECV`) is not determined. Eight connections: HTTP 429, then OVH dropped the SYNs of
    this address for about 20 minutes (`nettest get proof.ovh.net 80 /` timed
    out, other sites answered): four is the cap. `net` also stayed on `usb0`
    once with Wi-Fi up, its `wlan0` skipped for want of a DHCP answer in 10 s
    (the cause was not seen in a log: a chip glitch, or a lease lost without
    a switch, which now gets the same 10 s of grace). A skipped link is tried
    again after 60 s, then 120, 240... up to 16 minutes (in the emulator, a
    simulated Wi-Fi without DHCP made the console move to it every 70 s with
    a fixed 60 s: three tries in 330 s with the backoff). Ten
    `luna` orphans sat in `ps` (every `--reload display` since the last reboot
    left one: the kernel on the card has no `KILL_ON_CLOSE`), and one tap on a
    game card (the library was showing after a reload) started a real Luna
    session, ended by HOME ("the session could not be ended now").
63. 24/10/2026, same console, `luna` of `35926ab`: **lost video frames are
    recovered**. In the first run of the picture (run 62) every loss froze the
    picture until the next key frame: two minutes of Avatar gave 3493 frames,
    149 missing of which 96 "stale" (the server's retransmission, about 60 ms
    after the loss, completing a frame after its successors, which the
    decoder's numbering then dropped), 484 decoder errors, 134 key frames
    asked. The video channel had been released out of order, like the audio's.
    In order, waiting at most 120 ms for a gap: 243 frames recovered by
    retransmission, 42 given up, 91 errors, 41 key frames asked; at 250 ms:
    210 recovered, 25 given up, 75 errors, 31 key frames asked (about 2 s of
    freeze in 125 s). Audio unchanged (0 underruns in 4968 packets, 7 lost),
    decoding 14.8 ms a frame on average, 33 at most. The server did not send
    a second retransmission of a fragment lost twice; the FEC packets are the
    next thing to try.

63. 24/10/2026, same console and image as run 62, `fs` replaced in place
    (`usbterm.py --reload fs`, the fix of `388a2b7`): **a 17 MiB file reads
    right**. The fault of run 62 was in `fs`'s cache, not the card: a
    readahead batch could pick the same cache slot twice (the clock scan
    wrapped when every other slot was marked used), leaving a stale tag
    that later returned other sectors' bytes (damaged sectors 2048 apart,
    the slot count; copies of other sectors of the file). `stdtest bigfile
    write /bigpat.bin 17` (a pattern file, every sector unique) then
    `verify` 6 passes with `fs::read` and 3 passes in 3 KiB reads: 0 bad
    sectors in 9. The old `fs` was not measured on this pattern on the
    console (the emulator: 91 bad sectors in 12 passes before, 0 after).
    The shell's own `sync` failed with EPIPE once (its handle was the old
    `fs`'s), the next command worked; `luna` and `display` kept running.

62. 24/10/2026, same console and image as run 61 (gauge 67-69 %, plugged),
    `nxh264` and the GPU pass in `luna`: **the stream's picture shows on the
    panel**, with its sound. First attempt: mirrored (the panel's rows run the
    picture's columns backwards; one subtraction in the shader), then upright
    (Avatar's logos, title screen and menu read right, the colours match the
    desktop client). Announced at 60 Hz the server sent about 60 frames a
    second and the decoder (21 ms a frame with three threads at the boot
    clock) fell behind; announced at 30 Hz it sends about 29, which it
    honours. The CPU runs at 998 MHz (boot clock); `cores 1-3` showed "off"
    after hours idle (with them asleep the decoder's threads gained nothing:
    `cores f`). `h264bench` on 60 frames of a 15 Mbit/s 1080p x264 clip, ms
    a frame: at the boot clock one thread 185 and four threads 54 (the PC:
    17.8 and 7.5, a factor 10 and 7); with NEON luma interpolation and
    deblocking (16x16 block: 6-21 us before, 0.14-1.4 us after) and NEON
    inverse transforms, at 1219 MHz: one thread 64, four threads 21 (47
    frames/s). Live with four threads, the CPU at 1224 MHz for the stream
    and 30 Hz: 15.6 ms a frame on average, 34 at most, none dropped; what
    remains are lost frames on the network (4 %), each followed by a freeze
    until the next key frame. The CPU goes back to 998 MHz at the end of the
    stream (45 C, fan 22 %). Reading a 17 MB file from the card gave different
    bytes at every read (three CRCs, none the PC's) while a 1.5 MB file
    read right every time: a separate fault of the card's read path, not
    looked into.

61. 24/10/2026, Erista (gauge 15 %, plugged), rebooted, Luna of
    `9031b1e` put in `/services/luna/` over USB-C (3.1 MB in 8.4 s, after
    L had ended the old one): **the covers show**. The `HTTP 400` of runs
    58-60 was `nxhttp`'s `Content-Length: 0` on every GET, which Amazon's
    image origin refuses whenever its CDN has no copy of that size. First
    start, no library on the card: `87 new covers, first failure: none`,
    87 `cover-*.jpg` (about 15 KB each) and a 20 KB `library.json` in
    `/users/u1/services/luna`. Second start (L, R): `87 covers from the
    card`, `0 new covers`, one `luna` in `ps`. The home screen shows the
    portrait art filling each square, middle kept (the Demo catalog is gone
    from the card). No game was started (battery). Not measured: how long
    the covers take to show from the card.

60. 23/10/2026, Erista charged (gauge 16 %), boot image with the `fs`
    readahead fix (`23cf707`), Luna of `7fb2e75` (adaptive delay, audio
    out of SPP order), PC downloads paused: **no crash**, the catalog came
    up at its first start. Wobbly Life for ~105 s at 10 % volume. Play
    started with 3 packets in hand (40 ms after the first), and the first
    75 s had **0 lost, 0 late, 0 underruns**: 112 frames waited for (the
    delay grew to its 300 ms ceiling) and 94 dropped to shed bursts. The
    packets still came 0.4 to 1 s late at the 95th percentile, in bursts,
    while datagrams never stopped more than 177 ms and luna's thread was
    never busy: the server's audio comes in bursts over this network. In
    the last 30 s the PC's link broke down again (pings 328 ms on average,
    4.2 s at most; 192 audio packets lost on the network): 238 lost.
    nxopus: 285 us a frame on average, 0.67 ms at most.

    Cause found afterwards, in the emulator (not yet shown on silicon): `fs`'s
    sector cache read ahead into the shared I/O buffer, then took a victim
    slot for each sector; a victim with dirty data flushed through the same
    buffer and replaced the sectors still to copy (here at byte 837120 of a
    3 MiB read with a writer open). Any dirty sector (an open writer, a file
    being copied) made a large read return other bytes: not an instruction
    cache or DMA fault. Fixed in `user/fs/src/cache.rs` (victims taken before
    the read); `stdtest fs_cache` failed before and passes now.

59. 23/10/2026, Erista (battery above 15 %), boot image rebuilt at `23cf707`
    (`fs` with the readahead fix; the old pair saved as
    `/nxk/{bl33,bl31}.pre-fsfix`), `gpu` and `display` reloaded. The new
    `stdtest readcheck FILE CRC N` reads a file N times while a writer keeps
    dirty sectors: luna's 3,037,568-byte build, **40 of 40 reads had the PC's
    CRC**. Six `luna` starts from the shell (a fresh load each, 3 MB) ended
    without a fault. Not done: the same test on the old `fs` (it needs a
    second reboot), and the catalog running for a long time (network, SPP,
    where the faults came after a while).

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
