# nxk / L4T comparison

Both systems run on **the same emulated machine** (`tegra_qemu`,
`tegrax1` machine: 4 Cortex-A57, 4 GiB) and execute **the same
measurements**: same user code, same clock (`CNTVCT_EL0`), same
iterations.

## What is compared

| Side | Kernel | Measurement program |
|---|---|---|
| nxk | this repo | `user/bench` (nxk IPC: `call` / `reply_recv`) |
| L4T | Switchroot 5.1.2 Linux 4.9.140-l4t (May 2026), unchanged | `bench/lbench`, static `/init` with no libc (Linux pipes and futex) |

Measurements: null system call; request/reply round trip between two
threads (same core, two cores); throughput with 4,000-byte messages;
thread create + join; program spawn + wait (ELF loading included;
`vfork`+`execve`+`wait4` on the Linux side); one pure integer
computation that does not depend on the kernel and serves as a control;
memory in use after boot.

## How L4T boots in the emulator

On a real console, the chain is hekate → ATF (bl31) → U-Boot (bl33) →
kernel. In the emulator, `bench/l4tboot` replaces it: an EL3 monitor of
a few hundred lines that switches the GIC to non-secure mode, provides
PSCI 1.0 (including `CPU_ON`, with the core-wake sequence validated in
nxk) and jumps into the kernel with the device tree. Linux boots its
four cores via PSCI as on hardware.

The device tree is the Switchroot one (`nx-plat.dtimg`, 2017 model),
patched as `boot.scr` does: 4 GiB of RAM, UART-A as console, initramfs.
To boot headless in the emulator:

- `l4t-headless.disable` disables the devices the emulator does not
  model well enough for Linux (I2C PMIC and regulators, DVFS, screen,
  GPU, USB, audio, SD card, watchdog…). The measured kernel is
  unchanged: only device drivers are set aside.
- `cpuidle.off=1 idle=poll`: under the emulator, Linux cores put to
  sleep by `wfi` via the Tegra flow controller never wake up. With
  `idle=poll`, an idle core spins instead of sleeping. To compare on
  equal terms, nxk is also measured in `idle-poll`
  (`NXK_FEATURES=idle-poll ./tools/build.sh`).

## Reproducing

```bash
# nxk (3 runs)
NXK_FEATURES=idle-poll ./tools/build.sh
for i in 1 2 3; do ./tools/nxtest.py -c bench > nxk-$i.log; done

# L4T: Image, nx-plat.dtimg extracted from the Switchroot archive
python3 bench/mkinitramfs.py --init <lbench> -o initramfs.cpio
python3 bench/mkdtb.py --dtimg nx-plat.dtimg --initrd-start 0x92000000 \
    --initrd-size $(stat -c%s initramfs.cpio) \
    --disable "$(grep -v '^#' bench/l4t-headless.disable | paste -sd,)" \
    --bootargs "console=ttyS0,115200n8 rdinit=/init cpuidle.off=1 idle=poll" -o board.dtb
python3 bench/run_l4t.py --dir <directory> --until "RESULT END" --log l4t-1.log

python3 bench/compare.py --nxk nxk-*.log --l4t l4t-*.log --md bench/RESULTS.md
```

## Load test

`bench burn [SECONDS]` (60 by default) is not part of the comparison:
one thread per core runs NEON multiply-adds at the lowest priority, so
the shell and the services still answer. Every 10 s it prints the SoC
temperature and the fan (duty, rpm), then the rate reached
(`RESULT burn_mflops`). It stops early at a SoC of 85 C. Run it with
a trailing `&` to keep the shell, e.g. to watch `cpu` (0% idle).

## Limits

- **Emulation.** QEMU dynamically translates instructions: a kernel
  entry, an atomic instruction or a core wakeup costs far more there
  than on silicon, and caches are not modeled. Absolute values only hold
  for the emulator; the ratios between the two systems, measured under
  the same conditions, are the useful information.
- **Minimal L4T.** The L4T kernel is measured with a single user-space
  program. A real install boots Ubuntu with its graphical environment,
  which adds several hundred MB: the L4T memory figures are therefore a
  floor.
- **Different mechanisms.** nxk IPC (`call`/`reply_recv`) is compared
  against the two closest Linux mechanisms: pipes (the most common) and
  futexes (the fastest, with no data transport).
