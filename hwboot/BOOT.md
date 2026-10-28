# Booting nxk without hekate — plan

Goal: nxk starts the console itself (hardware init, load, hand-off), with no
third-party bootloader, and shows a boot screen from the first moment.
Status: in progress, see "Steps and status"; the console still boots
through hekate.

## What cannot go away

- **RCM injection stays.** On an Erista the BootROM only runs code signed by
  Nintendo, except through RCM (fusée-gelée). "No hekate" means nxk's own
  payload is injected (`fusee-nano`, or a dongle) at every cold boot. Writing
  our loader to the NAND is not possible (signed BCT) and is never done.
- **hekate stays on the SD card** as the recovery path and for the Wi-Fi
  firmware extraction (`README.md` "Wi-Fi firmware": Lockpick, Atmosphere).
- Every step keeps the hekate chain as a working fallback until the next one
  is validated on the console.

## What hekate does for nxk today (inventory)

| Left by hekate | Used by | In the IPL, or from reset? |
|---|---|---|
| Clocks, PLLs, BPMP/AHB setup | everything | IPL |
| PMIC (MAX77620) rails, CPU regulator (MAX77621) | `power`, `cpufreq` | IPL |
| DRAM init (SDRAM params for the board's RAM id), MC | kernel | IPL (needs hekate's table for Erista) |
| Minerva-trained EMC tables at `0xFFE80000` | `emc` (DRAM clock) | IPL: port the training, or ship fixed tables |
| Fuses `CFG_ALL_VISIBLE`, SKU | `clk` | IPL |
| SD card read, FAT32, loading `bl31.bin` + `bl33.bin` | boot | IPL: new bare-metal reader (read-only FAT32) |
| Panel lit (DSI, backlight), framebuffer | bl31 / kernel fbcon, `display` | IPL, via the `display` panel code (`display_init` already starts a panel from reset) |
| CPU cluster boot (PLLX, CPU rail, partitions, SB AA64 reset vector), TZDRAM | `nxk-bl31` | IPL |
| UART-B pinmux and clock (`uart_port=2`) | `bluetooth`/Joy-Con | drivers init from reset |
| SD slot powered off before the jump | `sd` | already handled (`sd/board.rs`) |
| IRAM / `PMC_SCRATCH110` magic | `nxk-bl31` | IPL |

## Decisions (Pierric, 2026-10-28)

Copy hekate's system, for the Erista and the Mariko as hekate does; an
injector at every cold boot is fine; dark boot screen; measured risks,
everything checked first. hekate v6.5.4 is the reference (`refs/hekate`,
`hwboot/fetch.sh`).

## How it is checked

`hwboot/oracle.sh` runs the real chain in the emulator from the RCM
payload on the emulated BPMP (hekate autobooting its L4T entry, retail
fuses with a chosen DRAM id) to nxk's shell, and can trace every register
access. `hwboot/tracecmp.py` compares the BPMP's accesses of hekate's run
and nxk-ipl's. Where the order legitimately differs (another SD driver,
the CPU started by nxk-ipl rather than NVIDIA's `bpmpfw.bin`), the check
becomes the registers' state at the CPU's start. `oracle.sh -g PORT`
starts a run paused under QEMU's gdb stub, which `hwboot/gdbrsp.py`
drives: the DRAM table at bpmpfw's and bl31's entries, a watchpoint, or
a hang written into nxk-ipl to fire its watchdog.

## Design: two stages

The payload must fit hekate's 128 KiB of IRAM; Minerva's tables alone are
103 KiB (three 7-entry tables of 4928 bytes), so, as hekate splits off its
modules:

- **Stage 1, `nxk-ipl.bin`** (RCM payload, IRAM): hw_init, DRAM, SD card,
  FAT32; loads stage 2, `nxk/bl31.bin` and `nxk/bl33.bin` into DRAM.
- **Stage 2, `nxk/ipl2.bin`** (DRAM, BPMP): panel and boot screen,
  Minerva training and its table at `0xFFE80000`, carveouts and the L4T
  state nxk-bl31 expects, then the CPU cluster (hekate's
  `ccplex_boot_cpu0`, its own path for HOS and Mariko L4T).

## Steps and status

1. Boot screen on the current chain: **done** (`bootsplash/`), not yet
   seen on the console.
2. The oracle and the trace comparison (`--state` too): **done**.
3. Stage 1 skeleton (relocation, UART-A, hekate's register names): **done**.
4. hw_init and DRAM init: **done**, identical to hekate's accesses for the
   8 Erista DRAM ids and the 10 Mariko DRAM codes.
5. SD card and FAT32: **done** (`sdcard/`, the `sd` service's own code,
   3.3 V default speed).
6. CPU start and hand-off: **done** in stage 1 (hekate's
   `ccplex_boot_cpu0`, L4T late config, carveouts equal to hekate's).
   nxk reaches its shell from nxk-ipl in the emulator, Erista and Mariko.
7. Panel and boot screen: **done** in stage 1 (every panel; DC, DSI,
   PWM, MIPI CAL, GPIO and pinmux state equal to hekate's); the screen
   shows 1 s after injection. 83 KiB of IRAM used with Minerva: no
   stage 2 so far.
8. Minerva port: **done** for the Erista. `nxk-emc` trains as hekate's
   `minerva_init` and `minerva_prep_boot_l4t` (its C run on the PC as an
   oracle: 54 training cases equal, register traces and table words),
   from `nxk/mtc_{0,2,7}.bin` (hekate's ODINX02 tables by DRAM id,
   `tools/mkemctab.py`, installed by `mksd-hw.sh`). nxk boots at 800 MHz
   with the table at `0xFFE80000` byte-identical to hekate's for the 8
   Erista DRAM ids, including the three PTSA rates NVIDIA's T210
   `bpmpfw.bin` rewrites in every entry before the CPU starts (found with
   a watchpoint over the emulator's gdb stub; nxk-ipl does not run
   bpmpfw). MC and EMC state at the CPU's start equal hekate's. With it
   come hekate's BPMP overclock (576 MHz, 589 on a Mariko; back to 408
   at the launch) and its watchdog: WDT4 for 2.5 s around training, whose
   FIQ restarts nxk-ipl without training (tested by writing a hang into
   training over the gdb stub). Mariko: no training, as hekate's
   table-less boost path and `bpmpfw_b01.bin`'s DVFS are not ported.
9. Battery: hekate's `_check_low_battery` ported (charger enabled at
   every boot, empty-battery screen and wait for the charger, power-off
   when not charging, the PMIC's low-battery shutdown armed again; the
   power-off path is not emulated). LP0 parameters in PMC scratch (SC7)
   and hekate's bl31 parameters (`SECURE_SCRATCH108`-`114`): not written;
   nothing in nxk reads them today, SC7 will need the LP0 block.
   State at the CPU's start otherwise equal to hekate's (registers, I2C
   transactions), but for the eMMC clock source and pad (hekate's eMMC
   init), UART-A (nxk-ipl's log), the BPMP exception vectors and the SD
   rail's power-up order (`nxk-sdcard`, same end state).
   UART-A's pads are internal on a console: nxk-ipl leaves a record in
   IRAM (`report.rs`) that nxk-bl31 prints, on the screen in text mode.
   Not checked on hardware: after a watchdog restart nxk-ipl starts the
   panel again with the backlight on (hekate's is still off there).
10. Console run, with Pierric's go: Erista first. Battery charged (no
    risk-free low-battery path yet on hardware), SD from `mksd-hw.sh`
    (hekate stays on it as the fallback), `nxk-ipl.bin` injected with
    Vol+ held for the text log: nxk-bl31's `nxk-ipl:` line should read
    800 MHz and 5 trained entries, and `emc` in the USB-C shell the same.

## Notes

- Rust targets for the BPMP's ARM7TDMI: `thumbv4t-none-eabi`, tier 3
  (`-Z build-std` under `RUSTC_BOOTSTRAP`, as `apps/`).
- Power-unsafe code (PMIC, DRAM voltage) uses hekate's exact values and
  order; a console run only with Pierric's go.
