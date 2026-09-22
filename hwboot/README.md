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

## Stage 2 : demarrer nxk via l'entree L4T de hekate (1er essai : aucune sortie visible)

Ni l'injection RCM ni `payload=` ne peuvent demarrer nxk : c'est une image
DRAM de ~1,7 Mo liee a `0x80000000` (`kernel/link.ld`), pas un payload
BPMP/IRAM (plafond ~196 Kio, DRAM non initialisee en RCM).

Chaine retenue (tout est construit et valide en emulateur, jamais sur
hardware) : **hekate `[nxk]` (L4T) → `nxk/bl31.bin` (moniteur nxk-bl31)
→ `nxk/bl33.bin` (= nxk.bin)**.

- `bootloader/ini/nxk.ini` : `l4t=1`, `boot_prefixes=nxk/`, `uart_port=2`
  (hekate active pinmux+horloge d'UART-B, rail Joy-Con droit ; moniteur et
  noyau ecrivent encore sur UART-A). Apparait dans More Configs.
- `nxk/bl31.bin` : construit par `mkbl31.py` depuis `hwboot/nxk-bl31/`
  (Rust `no_std`, ~12 Kio). Charge par hekate en TZDRAM a `0xFFF00000`, il
  pose sur chaque coeur les registres EL3 que le silicium laisse a une valeur
  inconnue (CNTFRQ_EL0, CPUECTLR_EL1.SMPEN, CPUACTLR_EL1), ouvre UART-A,
  efface l'ecran laisse allume par hekate et y recopie ses logs, met en
  marche le compteur systeme, bascule le GIC en Groupe 1, declare son Mo en
  carveout TZ (nxk l'exclut via `detect_holes`), recopie bl33 de
  `0xAA000000` vers `0x80000000`, refuse les `smc` (pas de PSCI -> nxk
  mono-coeur), et saute en EL2 (SCTLR_EL2, MDCR_EL2... dans un etat connu)
  avec x0 = 0 (sans DTB : DRAM via `MC_EMEM_CFG` + sondage).
- `nxk/bl33.bin` : copie byte-identique de `build/nxk.bin`.
- `bootloader/sys/l4t/` + `libsys_*.bso` : firmware BPMP/SC7/Minerva du
  pack Nyx (DRAM init par hekate) ; installes par `mksd-hw.sh` si absents.

Validation emulateur (`./test-bl31.sh`, meme binaire bl31 que sur hardware),
deux scenarios : sans ecran (banniere moniteur, recopie, `entree en EL2`,
`sans PSCI : un seul coeur`, invite `nxk>`) ; puis avec l'ecran tel que
hekate le laisse, reproduit par `fakehekate.py` (fenetre A du DC allumee
sur un faux logo a `0xF5A00000`) : l'image est relue en texte, le logo doit
avoir disparu et les lignes du moniteur puis du noyau y figurer jusqu'au
lancement de `init`. Le chemin EL2+PSCI 4 coeurs est aussi couvert par
`nxtest.py --monitor <l4tboot> --dtb build/moniteur.dtb`.

## Ecran : ce qu'on doit voir

Sans adaptateur UART, l'ecran est la seule sortie : hekate le laisse allume
en lancant une entree L4T, et le moniteur puis le noyau y ecrivent (texte
blanc sur noir, en portrait comme le menu texte de hekate : tenir la
console verticalement). Dans l'ordre :

1. Logo de hekate, puis une **ligne** qui se remplit en bas de l'ecran : le
   compte a rebours de hekate (`bootwait`), effacee juste avant le saut vers
   `bl31.bin`. Normal.
2. Ecran noir, lignes `[nxk-bl31] ...` : le moniteur tourne.
3. Banniere `nxk v0.1.0`, lignes `[mem]`, `[ecran]`, `[boot] ... lancement
   de init` : le noyau tourne. Une panique ou une faute du noyau s'affiche
   ici aussi.
4. Le service `display` reprend l'ecran : fond bleu nuit, une ligne
   `nxk - ecran 720x1280, terminal 90x80`. Espace utilisateur vivant ; la
   suite (shell) n'est que sur l'UART.

Photographier l'ecran la ou ca s'arrete : la derniere ligne situe la panne.
Si le logo reste affiche apres l'effacement de la ligne (1 sans 2), le
moniteur n'a pas tourne ou n'a pas trouve l'ecran.

Premier essai (22/09/2026, avant cette console d'ecran) : exactement ce cas
1 sans suite visible. nxk n'avait alors aucune sortie hors UART-A, et le
chemin materiel laissait des trous que QEMU masque, tous corriges depuis :
registres EL2/EL3 a valeur de reset inconnue (MDCR_EL2 peut pieger vers un
EL2 sans vecteurs les acces de nxk a OSLAR_EL1), verrous (acces exclusifs)
pris MMU coupee, carveouts du GPU de hekate non exclus de la memoire libre.

## Test hardware (quand la carte SD est de retour sur le PC)

1. `./mksd-hw.sh /mount/point` (reconstruit nxk + bl31, installe l'entree).
2. Remettre la carte dans la Switch, injecter hekate, **More Configs → nxk**.
3. Lire l'ecran (voir ci-dessus). L'adaptateur UART (rail Joy-Con droit,
   115200 8N1) reste necessaire pour le shell : moniteur et noyau ecrivent
   encore sur UART-A, que hekate n'a pas relie au rail.
4. Attendu : `entree en EL2`, `1/4 coeurs en ligne`, `lancement de init`,
   puis l'ecran du service `display`.
5. Recuperation : un boot rate se resume a un hang. Maintenir Power 12 s
   (extinction forcee), relancer en RCM, reessayer. Aucune ecriture NAND,
   eMMC ou SD par nxk : rien de persistant ne peut etre corrompu.

## Pourquoi nxk.bin ne peut pas etre injecte directement

Rappel : payloads RCM = BPMP en IRAM, ~196 Kio max, DRAM eteinte.
`build/nxk.bin` fait ~1,7 Mo (noyau + bootfs embarquee) et s'execute a
`0x80000000` en EL3/EL2 avec MMU coupee. D'ou le moniteur intermediaire,
qui fait ce que fait normalement ATF : GIC securise + saut EL2.
