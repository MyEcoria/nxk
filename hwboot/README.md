# hwboot — physical Switch (Erista, AutoRCM) bring-up kit

Host-side setup to boot a first-generation Switch into **hekate** over USB
(fusée-gelée RCM exploit). Stage 1 of running nxk on real hardware: today
nothing here boots nxk itself yet (see "Why nxk.bin can't be injected").

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

## Why nxk.bin can't be injected (stage 2 work)

- RCM payloads execute on the BPMP in IRAM with a hard size cap (~196 KiB).
  `build/nxk.bin` is ~1.7 MB (kernel + embedded bootfs) linked for DRAM at
  `0x80000000` (`kernel/link.ld`), entry `_start` in EL3 with MMU off.
- DRAM is uninitialized in RCM, so nxk needs a loader that does what
  hekate's BDK does (SDRAM init, `MC_EMEM_CFG`, clocks) and then releases
  the CCPLEX at `0x80000000`. Options: minimal BPMP loader reusing
  `refs/hekate/bdk` sequences, or chain hekate→U-Boot→`go 0x80000000`.
- Console visibility: nxk logs on UART-A (115200 8N1), which is **not**
  routed to USB on retail hardware — reading it needs a UART mod
  (Joy-Con-rail pads). The LCD panel init is also unimplemented
  (see README "Limites"). First hardware boots will be blind; validate
  with hekate's menu + SD activity, then bring up UART/display.
- So for now: success = hekate menu on the Switch screen via `./inject.sh`.
  Say the word and stage 2 (DRAM loader + blind-boot smoke test) is next.
