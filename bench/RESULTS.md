# Results: nxk vs L4T

Measurements from 21 September 2026. Same emulated machine
(`tegra_qemu`, Tegra X1, 4 Cortex-A57, 4 GiB), same measurement
programs, median of 3 runs per system. Method and reproduction steps:
`bench/README.md`.

- **nxk**: this repo, default configuration (idle cores in `wfi`).
- **L4T**: Switchroot 5.1.2 Linux 4.9.140-l4t (4 May 2026), kernel
  unchanged, official 2017 Switch device tree; `cpuidle.off=1` and
  non-emulated devices disabled (`l4t-headless.disable`).

| Measurement | nxk | L4T | Ratio |
|---|---:|---:|---:|
| Boot to first program (ms) | 596 | 5 560 | nxk x9.3 |
| Null system call (ns) | 1 646 | 2 085 | nxk x1.3 |
| Same-core IPC round trip (ns) - L4T: pipe | 4 927 | 41 828 | nxk x8.5 |
| Same-core IPC round trip (ns) - L4T: futex | 4 927 | 29 728 | nxk x6.0 |
| 2-core IPC round trip (ns) - L4T: pipe | 67 197 | 102 999 | nxk x1.5 |
| 2-core IPC round trip (ns) - L4T: futex | 67 197 | 113 271 | nxk x1.7 |
| Throughput, 4 KiB messages (MB/s) | 108 | 89 | nxk x1.2 |
| Thread create + join (us) | 226 | 836 | nxk x3.7 |
| Program spawn + wait (us) | 787 | 1 765 | nxk x2.2 |
| Pure computation, control (ms) | 66 | 60 | L4T x1.1 |
| RAM used by the system, kernel included (KiB) | 1 096 | 122 180 | nxk x111.5 |
| Kernel in memory (KiB) | 740 | 23 035 | nxk x31.1 |
| Measurement program RAM (KiB) | 64 | 52 | L4T x1.2 |

Ratio = how many times the first is better than the other.

## Reading

**IPC, where a microkernel is decided.** On the same core, an nxk round
trip costs 6x less than a Linux futex and 8.5x less than a pipe: two
system calls (`call`, `reply_recv`) and a direct thread-to-thread
switch, versus four calls and a trip through the CFS scheduler on the
Linux side. Across two cores the gap narrows (1.5 to 1.7) because both
pay for waking a sleeping core via IPI, which the emulator makes very
expensive.

**Null system call: 1.3.** The cost is mostly exception entry and exit,
identical for both; the gap comes from the kernel path itself (lock-free
fast path on the nxk side).

**Memory: two orders of magnitude.** After boot, the whole nxk system
(kernel, metadata, init, console driver, shell) occupies 1.1 MiB. The
L4T kernel alone, with a single user-space program, occupies 119 MiB:
23 MiB of resident image, ~64 MiB for the `struct page` table of the
4 GiB, reserved areas, 12.5 MiB of slab. On top come **512 MiB of CMA**
reserved for devices (usable only by movable pages).

**Control.** The pure computation, which does not depend on the kernel,
takes the same time on both sides (gap within emulator noise): both
systems were indeed measured under the same conditions.

## Limits

- **Emulation.** Absolute values only hold for the emulator (kernel
  entries, atomics and core wakeups cost far more there than on silicon;
  no caches). The ratios are the useful information.
- **Minimal L4T.** A real L4T install boots Ubuntu and its desktop:
  several hundred MiB more. The L4T memory figures are a floor, not a
  typical value.
- **Functional scope.** L4T ships thousands of drivers, a network stack,
  filesystems, a GPU. nxk today has only one driver (the serial
  console): the comparison covers kernel mechanisms, not the range of
  services provided.
