# Game card reader — status

What `gc` (`user/gc`, `sdcard/src/gc.rs`, the display Tools page) does today,
what was tried to go further, and why reading a cartridge's title and cover
is not implemented and will not be reverse-engineered from Nintendo's
firmware. See the `gc` row in [DEV.md](../DEV.md) for the technical detail
of the bring-up; this file is the status record the rest of this section
points to.

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
  everything down, reporting each step; nothing it sends can change the
  ASIC (see "Why there is no dump" below).
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

## What was tried, and where it stopped

The ASIC's bootrom (what `PROBE` talks to) accepts exactly one operation,
`SendFirmware`. Every other operation — including reading the card's
header, ID or partitions — needs the ASIC in its authenticated "secure
mode", reached through a handshake: an RSA-OAEP exchange, device
challenge/response, and a per-operation authentication field (`CvValue`)
that Nintendo's own documentation (Switchbrew's `Lotus3` page) explicitly
does not describe. Nothing public documents that handshake's order of
operations, key derivation, or `CvValue` computation; Atmosphère's open
source only has the card header's crypto, not the ASIC driver.

To see whether the pieces existed to describe it anyway, the project
extracted (from this console, with the owner's own keys, never
redistributed, never committed — `firmware/keys/` is git-ignored):

- `prod.keys` (Lockpick_RCM) and the console's own package2 and PRODINFO
  (Nyx's eMMC tools over USB mass storage).
- The console's firmware version (22.0.0) and its FS module, decompiled
  locally with Ghidra to look for the handshake's implementation.

That decompilation located the driver and confirmed the handshake is
real and reachable in principle. **It was stopped there.** Reconstructing
Nintendo's proprietary cartridge-authentication protocol from its
decompiled firmware — even for a legitimate homebrew OS, even with the
hardware owner's own keys — is reconstructing a DRM/anti-piracy
mechanism in order to defeat it. That is not something this project will
do, and no description of what was found in the decompiled code is kept
in this repository or in the assistant's own notes beyond "the driver
exists in FS and a handshake gates every operation past `SendFirmware`."

## Why there is no dump, and why none is planned

- `CMD60` operation `0x01` (`SendFirmware`) is never sent by `gc`: loading
  the ASIC's firmware burns OTP fuses (anti-downgrade), which is
  irreversible, and there is nothing past the bootrom to reach without
  the secure-mode handshake anyway.
- Reading the inserted cartridge's title and cover needs that same
  handshake. This project will not reverse-engineer it from Nintendo's
  firmware, so there is no path to that feature from here.
- A title/cover shown from a file prepared ahead of time (rather than
  read from the inserted cartridge) was considered and rejected: it
  cannot be tied to the cartridge that is actually in the slot, and could
  show the wrong game.

## What stays available

- `gc probe` / the Tools page keep reporting the bring-up (cartridge
  present, rail, reset, SDMMC2, tuning, CMD13) as a hardware diagnostic.
- `firmware/keys/` on this machine (PRODINFO, package2, the derived
  master keys, the decompiled FS) is the owner's own data, extracted for
  this investigation; it is not needed for anything nxk ships, and should
  be deleted once no longer wanted (`rm -rf firmware/keys`).
