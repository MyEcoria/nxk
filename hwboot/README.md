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

## Stage 2 : demarrer nxk via l'entree L4T de hekate (pret, en attente de test)

Ni l'injection RCM ni `payload=` ne peuvent demarrer nxk : c'est une image
DRAM de ~1,7 Mo liee a `0x80000000` (`kernel/link.ld`), pas un payload
BPMP/IRAM (plafond ~196 Kio, DRAM non initialisee en RCM).

Chaine retenue (tout est construit et valide en emulateur, jamais sur
hardware) : **hekate `[nxk]` (L4T) → `nxk/bl31.bin` (moniteur nxk-bl31)
→ `nxk/bl33.bin` (= nxk.bin)**.

- `bootloader/ini/nxk.ini` : `l4t=1`, `boot_prefixes=nxk/`, `uart_port=1`
  (hekate active pinmux+horloge d'UART-A). Apparait dans More Configs.
- `nxk/bl31.bin` : construit par `mkbl31.py` depuis `hwboot/nxk-bl31/`
  (Rust `no_std`, ~6 Kio). Charge par hekate en TZDRAM a `0xFFF00000`, il
  ouvre UART-A, bascule le GIC en Groupe 1, declare son Mo en carveout TZ
  (nxk l'exclut via `detect_holes`), recopie bl33 de `0xAA000000` vers
  `0x80000000`, refuse les `smc` (pas de PSCI -> nxk mono-coeur), et saute
  en EL2 avec x0 = 0 (sans DTB : DRAM via `MC_EMEM_CFG` + sondage).
- `nxk/bl33.bin` : copie byte-identique de `build/nxk.bin`.
- `bootloader/sys/l4t/` + `libsys_*.bso` : firmware BPMP/SC7/Minerva du
  pack Nyx (DRAM init par hekate) ; installes par `mksd-hw.sh` si absents.

Validation emulateur (`./test-bl31.sh`, meme binaire bl31 que sur hardware) :
banniere moniteur, recopie, `entree en EL2`, `sans PSCI : un seul coeur`,
invite `nxk>`. Le chemin EL2+PSCI 4 coeurs est aussi couvert par
`nxtest.py --monitor <l4tboot> --dtb build/moniteur.dtb`.

## Test hardware (quand la carte SD est de retour sur le PC)

1. `./mksd-hw.sh /mount/point` (reconstruit nxk + bl31, installe l'entree).
2. Remettre la carte dans la Switch, injecter hekate, **More Configs → nxk**.
3. **Adaptateur UART requis** (rail Joy-Con droit, ~5-10 €, sans soudure) :
   sans lui le premier boot est aveugle — ni l'ecran (init panneau DSI non
   implementee, voir README "Limites") ni l'USB ne montrent quoi que ce soit.
   Avec lui (115200 8N1 sur UART-A) : logs du moniteur puis banniere nxk.
4. Attendu : `entree en EL2`, `1/4 coeurs en ligne`, invite `nxk>`.
   `ktest` passera sauf `affinite et migration` (exige les coeurs 1-3).
5. Recuperation : un boot rate se resume a un hang. Maintenir Power 12 s
   (extinction forcee), relancer en RCM, reessayer. Aucune ecriture NAND,
   eMMC ou SD par nxk : rien de persistant ne peut etre corrompu.

## Pourquoi nxk.bin ne peut pas etre injecte directement

Rappel : payloads RCM = BPMP en IRAM, ~196 Kio max, DRAM eteinte.
`build/nxk.bin` fait ~1,7 Mo (noyau + bootfs embarquee) et s'execute a
`0x80000000` en EL3/EL2 avec MMU coupee. D'ou le moniteur intermediaire,
qui fait ce que fait normalement ATF : GIC securise + saut EL2.
