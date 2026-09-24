# Tasks

Planned work, not started. Each entry says what is known; per
`AGENTS.md` section 7, propose the options before coding any of them.

## 1. `usb`: alignment fault at every boot on the Switch

Since kernel reports reach the USB-C console (hardware run 18), every
boot on the Erista shows the `usb` (xHCI host) service dying:

```text
[nxk] 'usb' (pid 11, thread 11) : data fault at 0x40083650ff (read, alignment), pc 0x15b2713000 (+0x3000), sp 0x7fea2cf660, lr +0x2d0c -> process killed
[nxk] process 11 'usb' exited (code 139)
```

The address is in the MMIO window and ends in `0xff`: an unaligned
32-bit read of device memory, probably a register offset computed from
a value read back as all ones (hekate's L4T path leaves the xHCI
unpowered; `controller_ready()` in `user/usb/src/main.rs` asks
`libnx::clk` for the `XUSB_HOST` status first). It is the zombie `usb`
of earlier runs. The emulator does not reproduce it.

To do: find the faulting access (`pc +0x3000` in the `usb` binary),
explain why it happens on hardware, and make `usb` either stop cleanly
with a clear message or work.

## 2. `ktest` "priority preemption" fails in the emulator only

In `tegra_qemu`, `./tools/nxtest.py -c ktest` fails "priority
preemption" in about two runs of three (e.g. `max lateness of
high-priority thread: 8198 us`); it passes on the Switch, and it failed
the same way before the 2026-09-24 cleanup (commit `0f23868`).

Instrumenting `t_priority` showed only the first iteration is late:
8 to 10 ms, once 19 ms, i.e. one or two 10 ms quanta (`SLICE_NS`); the
nine others are 40 to 140 us late. Setup: a priority-25 thread spinning
pinned to core 2, a priority-5 thread pinned to core 2 sleeping 2 ms ten
times. QEMU runs MTTCG (its round-robin kick period is 100 ms, not the
cause).

To do: find whether the first deadline armed on that core is programmed
late (`timer::reprogram` and its `timer_cval` cache, `sched::arm`, the
quantum logic in `exit_to_user`) or whether it is a `tegra_qemu` timer
or GIC artefact, and fix the right side (nxk preferred).

## 3. `sd`: DMA straight into the client's buffer

Today `sd` programs ADMA2 into its own 256 KiB uncached DMA buffer, then
copies each block to or from the buffer `fs` attached (`blk::ATTACH`).
Programming ADMA2 directly on the client's physically contiguous buffer
saves those 256 KiB of uncached RAM and one `memcpy` per block: a real
performance gain for every card access.

What it involves: `fs` creates its I/O buffer contiguous and below
4 GiB (today `SharedMemory::new(256 KiB, 0)`), hands `sd` a handle with
the `DMA` right (`shm_paddr`), and `sd` keeps only its 4 KiB descriptor
page. To decide then: an uncached client buffer (slower CPU copies into
the `fs` cache) or a cached one with cache maintenance around each
transfer.

Risk: it changes the hardware DMA path, so it must be tested on the
real Switch (reads and writes, `stdtest` `files`, a large `--put`), not
only in the emulator.
