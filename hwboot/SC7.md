# Deep sleep (SC7) on the Erista: feasibility study

Goal: replace the software sleep (1.67 W with Lumen held, `hwboot/README.md`
runs 48-49) by a SoC deep sleep, DRAM in self-refresh, CPU and SoC rails off,
woken by the Power button, the game going on where it stopped.

Sources: hekate `bootloader/l4t/l4t.c` (the L4T launch nxk already uses),
Atmosphère `exosphere/program/source/smc/secmon_smc_power_management.cpp`,
`exosphere/program/sc7fw/` and `libexosphere/source/pmc`, Linux
`drivers/soc/tegra/pmc.c`; the disassembly of hekate's `sc7entry.bin`.

## 1. What hekate's L4T launch already leaves for a suspend (Erista, T210)

Every nxk boot goes through `launch_l4t()`, which refuses to start unless
`bootloader/sys/l4t/` holds `sc7entry.bin`, `bpmpfw.bin` and `sc7exit.bin`
(each tagged `VER7`, the Nyx pack on the card). Before jumping it:

| item | where | what |
|---|---|---|
| `sc7entry.bin` (1764 B) | DRAM `0xFFE00400`, 0x400 B header at `0xFFE00000` | BPMP (ARM7) code: ATF copies it to IRAM `0x40000000`, starts the BPMP there, waits for its sign of life in the resource semaphore; it then waits for the CPU rail to go off, puts DRAM in self-refresh, DDR pads in DPD, triggers LP0. Its first lines: SP = `0x40002000`, EVP COP vectors, all six ICTLR COP IER cleared (same as Atmosphère's sc7fw). |
| `sc7exit.bin` (2420 B) | DRAM `0xFFE10000`, `PMC_SCRATCH1` = that address | warmboot firmware the boot ROM loads on LP0 exit; hekate copies the console's RSA modulus from its BCT (eMMC BOOT0) into it at +0x10 (`pkg1_warmboot_rsa_mod`), so the key hash matches the fuses. |
| `bpmpfw.bin` (101 KB, NVIDIA LK, mailbox) | DRAM `0xFFEC0000`, entry +0x100 | on T210 it is what starts the CCPLEX at `0xFFF00000` (our bl31); it then idles on the BPMP. |
| MTC table | DRAM `0xFFE80000` | Minerva's trained EMC tables (all frequencies), passed to BL31 as `emc_table_base`. |
| SECFW carveout | MC GSC1 = `0xFFE00000`, 1 MiB | BPMP, CCPLEX and AHB R/W. nxk's `detect_holes` already keeps it out of free DRAM, so the blobs survive. |
| LP0 SDRAM params | PMC scratch | saved by hekate at its DRAM init; MC carveouts (TZ, GSC1-5, VPR) packed into secure scratch by `_l4t_sdram_lp0_save_params` for SC7 exit. |
| `PMC_SCRATCH201` = BIT(1) | PMC | "CPU PMIC is a MAX77621", read by the SC7 firmware to cut VDD_CPU. |
| PLLM override cleared | `PMC_PLLP_WB0_OVERRIDE` | so the boot ROM brings PLLM up from the boot BCT on exit. |
| `LOCK_PMC_REGISTERS` 0 | | no scratch is write-locked: bl31 can still write `PMC_SCRATCH0/1/31/32`, `SECURE_SCRATCH34/35`. |

What is **not** prepared, ATF's job at suspend time: the PMIC sleep
(MAX77620 FPS), the PMC wake mask/levels, `PMC_SCRATCH0` = 1 (warmboot
flag), the CPU resume vector, starting `sc7entry.bin` on the BPMP, and on
Erista the boot ROM patch that lets a non-Nintendo warmboot run
(`PMC_SCRATCH31` = `0x2202E012`, `PMC_SCRATCH32` = `0x6001DC28`, what
exosphere writes right before powering off).

## 2. How exosphere does SYSTEM_SUSPEND (`SuspendCpuImpl`)

1. power state must be "power down", state id 27 (SC7);
2. charger to Hi-Z if asked (I2C1);
3. `pmc::EnableWakeEventDetection`: `CNTRL2.WAKE_DET_EN`, clear `WAKE_STATUS`,
   `WAKE2_STATUS`, 75 us waits;
4. I2C5 up, `pmic::EnableSleep` (MAX77620: sleep enable, FPS);
5. checks: cores 1-3 power-gated (`PWRGATE_STATUS` CE123 = 0), BPMP halted,
   USB/DMA in reset, IRAM/GPIO/FUSE/ACTMON clocks on;
6. `pmc::ConfigureForSc7Entry`: `PMC_SCRATCH0` = 1, `DPD_ENABLE.TSC_MULT_EN`;
7. flow controller for this core: CC4 0, no halt events, L2 flush cleared,
   `CPU_CSR` = ENABLE_EXT_POWERGATE_CPU_TURNOFF_CPURAIL;
8. save entry point/context, debug registers; secure context (Erista: SE
   context + TZRAM encrypted to DRAM, keys in secure scratch: HOS only);
9. load `sc7fw` into IRAM, COP vectors, release the BPMP;
10. Erista: the two scratch writes above; then flush and `wfi`.

`sc7fw` on the BPMP: wait CRAIL off, clamp it, I2C5 → MAX77621 VDD_CPU off,
mask all ICTLR, save the LPDDR4 FSP (MRW3 → `SCRATCH18/12/13`), DRAM
self-refresh (dyn-SR off, DLL off, ZQ/auto-cal off, stall, `EMC_SELF_REF`),
PASR all segments, DDR DPD (`DDR_CNTRL`, weak bias, `IO_DPD3/4_REQ`),
`DPD_SAMPLE`, then `DPD_ENABLE` = deep sleep.

Wake on the Switch: the Power button is the MAX77620's ONOFF (nxk's `input`
polls `ONOFFSTAT` over I2C today); the PMIC's IRQ line is PMC wake event 51
("pmu", GIC SPI 86 in Linux). The PMC wakes, raises PWR_REQ, the PMIC
restores its rails, the boot ROM restores DRAM from the scratch params,
loads and checks the warmboot at `PMC_SCRATCH1`, which restarts the CCPLEX at
the vector in `PMC_SECURE_SCRATCH34/35` (ATF's convention; to confirm in
`sc7exit.bin`).

## 3. What SC7 means for nxk

Only DRAM and the PMC (always-on) keep their state. Everything else is reset:

- **bl31**: no SYSTEM_SUSPEND, no CPU_OFF, no warm entry. Needs: CPU_OFF,
  SYSTEM_SUSPEND (steps above), a warm `_start` path that recognises a
  resume (EL3 regs, GIC secure init, SB reset vector, system counter) and
  enters nxk's resume entry in EL2.
- **kernel**: cores 1-3 off (hotplug), core 0 saves its EL2/EL1 system
  registers and stack, GIC distributor state (lost with the CPU rail), a
  monotonic clock that does not go back (the system counter restarts), timers
  re-armed, cores 1-3 back with CPU_ON.
- **EMC**: the boot ROM restores DRAM at the BCT boot frequency (204 MHz).
  Getting back the boot speed needs an EMC clock change with hekate's trained
  table (`0xFFE80000`): a port of Minerva / Linux `tegra210-emc-cc-r21021`.
  Without it the game resumes at a fraction of its memory bandwidth.
- **services**: every one must re-initialise its hardware from reset on
  `RESUME`, not just ungate clocks. The hard ones:
  - `display`: DC, DSI, PLLD and the panel came from hekate (run 49). The
    service now has its own bring-up and power-down (`user/display/src/panel/`,
    hekate `di.c`, panel id-specific sequences; run on the Erista,
    `README.md` run 53), which is what an SC7 exit needs
    to see anything after the wake.
  - `gpu`: full re-init (PMU, ACR/WPR, FECS/GPCCS firmware) and the game's
    channel and context rebuilt, transparently to the game.
  - `sd`, `usbcon` (the cable re-enumerates), `wlan` (PCIe and firmware),
    `bluetooth`, `audio`, `joycon`, `i2c`, pinmux and GPIO, `clk`'s PLLs.
- **measurement**: the CPU is off, `powertest` cannot sample at 1 Hz; read the
  gauge's accumulated charge (MAX17050 QH, ~1 mAh step) before and after a
  long sleep, cable out (or HIZ with the BQ24193 watchdog disabled).

## 4. Options

**A. Full SC7 through nxk-bl31 and hekate's blobs.** bl31 gains CPU_OFF,
SYSTEM_SUSPEND and a warm entry; it runs `sc7entry.bin` on the BPMP and relies
on `sc7exit.bin` for the exit. Best result (tens of mW, like HOS sleep), but
it only works end to end once all services re-initialise from reset, display's
native panel init and the EMC clock change included: weeks of work, each step
a console reboot to test, and blobs whose exit protocol is to be confirmed by
disassembly.

**A'. Same, with our own BPMP code (port of Atmosphère's sc7fw / warmboot).**
Readable, fixable, no blob protocol to guess; needs an ARMv4T build
(`armv4t-none-eabi`, `-Z build-std=core`) and the Erista boot ROM patch.
Same service prerequisites as A.

**B. "LP1": CPU rail off, DRAM self-refresh, SoC core rail on.** Our own BPMP
code in IRAM enters self-refresh (sc7fw's sequence minus DPD/LP0), waits for
the Power button (PMIC IRQ, LIC line 86, or ONOFFSTAT polled over I2C5), leaves
self-refresh, powers VDD_CPU and CE0 back; core 0 restarts at bl31's warm
entry, which returns into the kernel. Device state is kept: no service
re-init beyond today's `SUSPEND` plus stopping every DMA (DC fetch, GPU,
USB, ADMA, PCIe) during self-refresh. Removes the CPU rail, EMC clock, PLLM,
PLLX; keeps VDD_CORE leakage. Reuses ~70 % of what A needs (bl31 CPU_OFF and
warm entry, kernel context save/restore, GIC restore, BPMP firmware, the
self-refresh sequence).

**C. No suspend: scale things down.** CPU_OFF for cores 1-3, EMC DVFS to
204 MHz while asleep (Minerva port), GPU rail off (needs the GPU re-init).
Each piece is reusable by A, but the CPU rail and DRAM stay on.

Common first stage whatever the choice: **PSCI CPU_OFF and kernel core
hotplug** (cores 1-3 off asleep, back at wake). Exosphere refuses SC7 unless
CE1-3 are gated; it is testable in the emulator and on the console without a
new bl31 protocol beyond CPU_OFF.

## 5. Checks made before choosing

- **SC7 exit vector** (disassembly of `sc7exit.bin`, loaded by the boot ROM
  at IRAM `0x40010240`): it copies `PMC_SECURE_SCRATCH34` | 1 to
  `SB_AA64_RESET_LOW` and `SECURE_SCRATCH35` to `SB_AA64_RESET_HIGH`, brings
  VDD_CPU back over I2C5 (its own I2C driver), releases core 0
  (`RST_CPUG_CMPLX_CLR` = 0x00010001), then halts the BPMP
  (`HALT_COP_EVENTS` = 0x50000000). It does not go back through
  `bpmpfw.bin`, and hekate's unused `TZDRAM_WARM_ENTRY` (+0x200) is not
  involved: bl31 picks its warm entry by writing SECURE_SCRATCH34/35.
- **Its signature block is zero**: only the modulus is patched in, so on
  Erista the boot ROM accepts it only with exosphere's two scratch writes
  (`SCRATCH31`/`32`), which bl31 must then make at suspend.
- **EMC**: hekate's `minerva_prep_boot_l4t` trains every table, then leaves
  the EMC at **800 MHz** (not 1600); nxk has no EMC code, so it runs there.
  To confirm on the console from a boot log line (`CLK_SOURCE_EMC`, PLLM),
  with the BPMP state (`HALT_COP_EVENTS`, COP reset, `EVP_COP_RESET_VECTOR`):
  nxk has no shell access to these registers.
- **IRAM**: nothing in nxk uses it (usbcon's queue heads are in the USB
  controller's own RAM, `0x7D001000`); `sc7entry.bin` needs
  `0x40000000-0x40002000`.

## 6. Decision (advisor review, AGENTS.md section 7)

**SC7 stays the target (option A, hekate's blobs for entry and exit)**: the
exit path is self-contained and already in DRAM at every boot, and the result
(tens of mW) is far better than B or C. **B is a milestone, not an
alternative**: nearly all of it (bl31 CPU_OFF and warm entry, kernel context
and GIC save/restore, BPMP start, self-refresh) is on SC7's critical path.
A' (our own BPMP code) stays the fallback if a blob misbehaves.

Stages, each a runtime opt-in so one boot image covers several trials and
today's software sleep stays the default:

1. **CPU_OFF and kernel core hotplug.** Cores 1-3 off asleep, back at wake.
   bl31: CPU_OFF as exosphere (flow controller `CPUn_CSR` power-gate on WFI,
   L1 flush, SMPEN off, GIC CPU interface off), CPU_ON waits for a pending
   gate, the woken core clears its flow-controller registers. Kernel: the
   dying core moves its threads, deadlines, FP state and interrupt lines to
   core 0, then calls CPU_OFF; `power` asks for it, `display` at sleep.
   **Done** (`README.md` run 50): on silicon a core the flow controller
   gated comes back by itself once `HALT_CPUn_EVENTS` is cleared (exosphere's
   PMC toggle does not bring it back); asleep with Lumen held, -65 mW
   (1.561 W -> 1.496 W). Still open from section 5: the boot line with the
   BPMP and EMC state scrolled out of `display`'s log before it was read.
2. **Core 0 power-down rehearsal** with the CPU rail on, woken by a Tegra
   timer (the arch timer dies with the core): warm entry, EL2/EL1 and GIC
   distributor restore, no DRAM or PMIC risk.
3. **SC7** proper: PMIC sleep (FPS), wake mask on event 51 only (PMIC
   IRQ, ONOFF unmasked in the MAX77620), `sc7entry.bin` on the BPMP, warm
   entry through SECURE_SCRATCH34/35, the Erista boot ROM scratch writes;
   first with only the console (`usbcon`) re-initialised.

Long poles that do not depend on bl31, each its own piece of work: a native
DSI and panel init in `display` (hekate `di.c`; done, `README.md` run 53), a GPU cold restore with a
game held, an EMC clock change from the trained table (back from the boot
ROM's 204 MHz to 800 MHz; written as the `emc` service, `hwboot/README.md`
"DRAM clock": it also drops the DRAM to 204 MHz asleep today), and every other service re-initialising from reset
on `RESUME`.
