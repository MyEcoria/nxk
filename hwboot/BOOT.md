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

## Steps

1. **Boot screen on the current chain** (no new power code). `nxk-bl31`
   draws the nxk logo and a progress bar instead of text; the kernel console
   leaves it alone; `display` continues from the same frame with an
   animation until HOME. Text diagnostics (panic, fatal error) still draw
   over it. Needs `./hwboot/test-bl31.sh` and a console reboot to confirm.
2. **Fewer hekate seconds.** `autoboot` to the nxk entry with a minimal
   `bootwait` (keys to check against hekate's own docs; `mksd-hw.sh` keeps
   the ini already on the card, so a repo edit alone does not reach it).
3. **IPL "top half", run as a hekate `payload=`.** DRAM is already up. Develop
   the SD reader, cluster boot and the bl31 hand-off with a visible screen.
   Check whether hekate leaves the display lit for payloads.
4. **IPL "bottom half", started by RCM directly.** BPMP clocks, PMIC, then
   `sdram_init` for the Erista RAM ids, then panel bring-up. Factor
   `display/panel` and `sd/board.rs` into shared `no_std` crates. Debug
   channel before DRAM: a PMC scratch progress code read after a return to
   RCM (a 720x1280 framebuffer does not fit in IRAM, UART pads are internal).
5. **Minerva port** (training), so the DRAM clock keeps its tables; without
   it the DRAM stays at one rate and frame rate suffers.
6. **Drivers stop assuming hekate's leftovers** (fuses, UART-B, xHCI...).
7. **Mariko**, only if in scope (different PMIC, SDRAM, no fuse visibility
   quirks; RCM through a modchip).

## Feasibility

- Hekate's source is on disk (`../SwitchMcp/nxk/refs/hekate`); this
  checkout's `refs/` is missing and must be restored before porting.
- Rust targets for the BPMP's ARM7TDMI exist (`armv4t-none-eabi`,
  `thumbv4t-none-eabi`, tier 3: `-Z build-std`, already needed by `apps/`).
- IRAM budget for the payload is under about 196 KiB: DRAM tables only for
  the Erista ids.
- The emulator cannot validate steps 3-5 (no BPMP payload, no real DRAM
  training). Register-trace tests against hekate's C (the method `emc/`
  uses) cover the sequences; the rest is checked on the console.
- Power-unsafe code (PMIC, DRAM voltage) is hardware-risky: safe defaults,
  hekate's exact values, a console run only with Pierric's go.

## Decisions pending (Pierric)

Erista only or Mariko too, an injector at every boot or a dongle, permission
to inject experimental IPLs, splash style (white as the Switch, or the dark
theme). Listed in the thread.
