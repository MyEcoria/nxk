# Booting nxk without hekate — plan

Goal: nxk starts the console itself (hardware init, load, hand-off), with no
third-party bootloader, and shows a boot screen from the first moment.
Status: plan only. Step 1 (boot screen) is the only one that touches no
clock, voltage or DRAM code.

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
becomes the registers' state at the CPU's start.

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

1. Boot screen on the current chain: **done** (`bootsplash/`, branch
   `feat/boot-screen`), not yet seen on the console.
2. The oracle and the trace comparison: **done**.
3. Stage 1 skeleton (relocation, UART-A, hekate's register names): **done**.
4. hw_init and DRAM init: **done**, identical to hekate's accesses for the
   8 Erista DRAM ids and the 10 Mariko DRAM codes.
5. SD card and FAT32 in stage 1: `user/sd`'s card logic (Rust, run on the
   Erista) moved into a shared `no_std` crate; 3.3 V default speed first
   (about 0.4 s per boot), faster modes later for both users. The BPMP
   cache must be invalidated after each DMA read (the emulator does not
   model it).
6. Stage 2: CPU start and the state at handoff compared with hekate's.
   First milestone: nxk's shell from nxk-ipl in the emulator.
7. Panel bring-up and the boot screen from stage 2 (`display/panel`
   moved into a shared crate).
8. Minerva port. Until then the DRAM stays at its boot rate: the hekate
   chain stays the console's boot path, and `emc` must refuse a missing
   table at `0xFFE80000`.
9. LP0 parameters (sleep), low-battery check, watchdog, as hekate.
10. Console run, with Pierric's go: Erista first.

## Notes

- Rust targets for the BPMP's ARM7TDMI: `thumbv4t-none-eabi`, tier 3
  (`-Z build-std` under `RUSTC_BOOTSTRAP`, as `apps/`).
- Power-unsafe code (PMIC, DRAM voltage) uses hekate's exact values and
  order; a console run only with Pierric's go.
