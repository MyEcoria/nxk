# Game card reader — status

What `gc` (`user/gc`, `sdcard/src/gc.rs`, the display Tools page) does today.
See the `gc` row in [DEV.md](../DEV.md) for the technical detail of the
bring-up; this file is the status record the rest of this section points to.

## Done (in `dev`, commits `f5c599a`..`195ec2f`, `932e09d`, `ce8138e`)

- **`abi`**: `clk::SDMMC2` and `proto::gc` (`STATUS`, `PROBE`, the `ST_*`
  bits a probe reports).
- **`user/clk`**: SDMMC2's clock from PLLC4 (hekate's dividers, 199.68 MHz
  on OUT2); PLLC4 is only stopped again if this service started it.
- **`sdcard`** (crate `nxk-sdcard`, module `gc`): the SDMMC2 host at 8
  bits, 1.8 V, HS100 timing, hardware tuning on CMD21, and a CMD13 status
  read — hekate's `sdmmc_storage_init_gc` sequence, additive to the
  existing SD card code. Host-tested on a fake register file
  (`cd sdcard && cargo test`).
- **`user/gc`**: the cartridge detect switch (GPIO S3, pad `CAM_I2C_SDA`),
  the ASIC's rail (MAX77620 LDO3, 3.1 V) and reset (GPIO BB3, pad
  `GPIO_X1_AUD`), wired from Atmosphère's GPIO tables and confirmed
  against Linux's `pinctrl-tegra210.c`. `PROBE` powers the ASIC, releases
  the reset, brings SDMMC2 up, tunes it, reads CMD13, then powers
  everything down, reporting each step.
- **`user/shell`**: `gc` (status) and `gc probe`.
- **`user/display`**: a Tools round button on HOME and a Tools page with
  the cartridge's state, a Probe button and the steps reached.
- **Hardware run 81** (Erista, `hwboot/README.md`): the probe passes in
  full with a cartridge in — rail, reset, SDMMC2 at HS100, tuning, and
  CMD13 (`0x804`) all succeed in 46 ms. The detect switch reads low with
  the cartridge in, high with it out (checked by removing it). The Tools
  page works on the console's own panel.
- **Boot without hekate**: `hwboot/inject.sh` now sends `nxk-ipl.bin` by
  default; `AGENTS.md`, `README.md`, `DEV.md` and `hwboot/README.md` say
  nxk starts itself. hekate stays on the SD card for recovery and for
  extracting keys and firmware (Lockpick, Nyx's package1/2 and PRODINFO
  tools), never to boot nxk.

## Status

The probe is the whole of the `gc` service: it reports the cartridge's
presence and the bring-up steps as a hardware diagnostic (`gc probe`, the
Tools page). The contents of the inserted cartridge are not read.
