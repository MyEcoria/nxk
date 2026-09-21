# AGENTS.md — nxk

This file guides AI coding agents working on **nxk**. Follow it on every task
(dev, debug, docs, benchmarks).

## 1. What nxk is

- Microkernel in **pure Rust**, no host OS, for NVIDIA Tegra X1 (Nintendo
  Switch Erista / Mariko: 4x Cortex-A57).
- EL1 keeps only: address spaces, threads, scheduler, IPC, capabilities,
  IRQ routing, futex. Everything else (drivers, fs, net, services) runs as
  ordinary **EL0** programs with only the capabilities granted to them.
- User programs are **ordinary Rust with `std`** on target
  `aarch64-unknown-nxk` (see `std/` + `tools/mkstd.py`). `user/libnx` is the
  `no_std` system library underneath `std`.
- Everything is validated **only in the `tegra_qemu` emulator** (sibling
  directory, QEMU fork). Never assumed to run on real hardware.
- Syscall convention: `svc #0`, number in `x8`, args in `x0`–`x5`, result in
  `x0`, errors as `-errno` (Linux values). Single source of truth: `abi/`
  (`nxk-abi` crate shared by kernel, libnx, std).
- Existing code and comments are mostly in **French**. Keep new comments
  consistent with surrounding files; docs for agents and commit messages
  may be in English.

## 2. Layout

```text
abi/        nxk-abi: syscall numbers, errors, rights, service protocols
kernel/     the EL1 microkernel (crate `nxk`)
user/       workspace: libnx (no_std) + init, shell, drivers, services, tests
std/        Rust std port: std/library overlay, target JSON, linker script
apps/       ordinary std programs: hello-std, stdtest
bench/      nxk vs L4T comparison (lbench, scripts, RESULTS.md)
tools/      build.sh, mksd.py, run.sh, debug.sh, nxtest.py, mkstd.py, ...
refs/       hekate / Atmosphere — hardware documentation only, never compiled
build/      generated artifacts (nxk.elf symlink, bootfs.img) — git-ignored
```

Kernel internals: `kernel/src/{arch,drivers,kern,mm,sync,boot.rs,...}`.
User services: `user/{console,clk,i2c,power,sd,fs,display,input,usb,net,init,shell,...}`.
Tests: `user/ktest` (kernel tests, ~27), `apps/stdtest` (std tests, ~18).

`tegra_qemu/` (sibling of `nxk/`, not covered by this file) is an external
dependency. Prefer fixing nxk side; touch the emulator only when the bug is
clearly in device emulation, and say so explicitly.

## 3. Mandatory workflow: build, run, test

Build directories live **outside the repo** in `~/.cache/nxk/`
(`$NXK_CACHE` overrides). Never commit `target/` or `build/` contents.
Prerequisites: stable Rust + `rust-src`, `aarch64-unknown-none` and
`aarch64-unknown-none-softfloat` targets, compiled `tegra_qemu`, `mkfs.fat`,
`pyfatfs`.

```bash
./tools/build.sh                  # user progs -> bootfs -> std chain -> kernel
./tools/mksd.py -o build/sd.img   # FAT32 SD image (/bin/*)
./tools/run.sh                    # build + boot; shell on stdio (Ctrl-A X quits)
./tools/nxtest.py -c ktest -c stdtest --sd build/sd.img
```

Key details agents must respect:

- `tools/build.sh` is the canonical build: user workspace
  (`CARGO_TARGET_DIR=$cache/target-user`), then std chain
  (`tools/mkstd.py`, toolchain named `nxk`), then apps with
  `RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=$cache/target-apps`, then
  `mkbootfs.py`, then kernel
  (`CARGO_TARGET_DIR=$cache/target-kernel`). Do not rebuild pieces by hand
  in repo-local `target/` dirs.
- `apps/` requires the `nxk` toolchain (`apps/rust-toolchain.toml`,
  `channel = "nxk"`) prepared by `mkstd.py` (hardlinked stable Rust + ported
  std, `-Z build-std`; see `apps/.cargo/config.toml`). If `std/` changed,
  `mkstd.py` invalidates the apps cache — always go through `build.sh`.
- Kernel target is `aarch64-unknown-none-softfloat`, `target-cpu=cortex-a57`
  (see `kernel/.cargo/config.toml`). Kernel and user profiles use
  `panic = "abort"`, LTO, `codegen-units = 1`.
- QEMU path: `$QEMU`, default `~/.cache/tegra_qemu/build/qemu-system-aarch64`.
  Fail fast if missing.
- `nxtest.py` drives UART-A over a socket, waits for the `nxk>` prompt, types
  `-c` commands, exits 1 on timeout. Useful flags: `--sd`, `--machine
  tegrax1plus` (Mariko), `--monitor ELF --dtb DTB` (EL2 under PSCI monitor),
  `--net [hostfwd...]`, `--usb-kbd --keys`, `--screendump out.ppm`,
  `--timeout SECS`. Extra QEMU args via `NXTEST_QEMU_ARGS`.
- **Strict rule: after any kernel / libnx / service / std / app change, run
  `./tools/build.sh` then `./tools/nxtest.py -c ktest -c stdtest --sd
  build/sd.img` (add relevant `-c` commands, e.g. `-c bench`,
  `-c "run hello"`). Paste failures verbatim; on boot timeout use the
  per-CPU `PC/EL` dump `nxtest.py` prints (via `nm` symbolization) to
  localize hangs.
- Debugging: `./tools/debug.sh` (like `run.sh` plus `-s -S`, GDB on `:1234`):
  `aarch64-none-elf-gdb build/nxk.elf -ex 'target remote :1234'`. Kernel keeps
  debug symbols (`debug = true` in release profile).
- Benchmarks: see `bench/README.md`. For L4T-comparable numbers build with
  `NXK_FEATURES=idle-poll ./tools/build.sh`. Reproduce with
  `./tools/nxtest.py -c bench`; compare via `bench/compare.py`.

## 4. Architecture rules (do not break)

- No GUI: shell over serial console only.
- EL1/EL0 split: no driver or filesystem logic in the kernel. Services get
  least-privilege capabilities; designate objects only via handles with
  rights + generation (revocation by generation bump).
- Kernel style: one kernel stack per core (seL4 model, no mid-call blocking),
  big global lock + lock-free fast path for stateless calls (`clock`,
  `getcpu`, `gettid`), tickless 32-priority O(1) scheduler, synchronous IPC
  (6 words in regs + up to 4 KiB UTCB + up to 4 handles), timeouts on all
  blocking calls, per-core run queues with idle-core balancing.
- Memory: buddy allocator, `DC ZVA` zeroing, ASID-tagged ASs, demand paging
  (incl. multi-core races), enforced W^X, ASLR for static-PIE / heap / stacks,
  MMIO/DMA via `shm_*` + `mmio_map`/`mmio_derive`, exclude MC carveouts and
  `/reserved-memory`.
- Security: Spectre-v2 mitigation on process switch (default feature),
  faulty program is killed without taking down the rest.
- `std` support matrix in `README.md` ("Ce que fournit `std`") is normative:
  extend it rather than regress it (no unwinding / backtraces: `panic=abort`).
- `abi/` is the single source of truth for syscall numbers and protocols.
  Never duplicate numbers in kernel/user/std.
- Pipes (`endpoint_create` tube mode) are unbuffered rendezvous: writer blocks
  until a reader takes the data (`Command::output` reads both outputs in
  parallel for this reason). `fs` refuses to delete an open file (`EBUSY`).

## 5. Coding conventions

- Rust edition 2021, `cargo fmt` clean, `cargo clippy` warning-free for new
  code. Keep the existing terse, comment-in-French style; explain *why* for
  hardware quirks (GIC Group 0→1 from EL3, secondary-core wake sequence:
  SB reset vector → PMC power partition → voltage locks → CAR resets).
- `no_std` in `kernel/` and `user/libnx`; ordinary `std` in `apps/` and most
  of `user/` services where already used. Do not add external crates without
  need; check `Cargo.lock` impact and offline/air-gapped builds.
- New user program: create crate under `apps/` (std) or `user/` (system),
  register in workspace `Cargo.toml` **and** in the `progs`/`apps` lists in
  `tools/build.sh`, plus SD install if needed (`tools/mksd.py`).
- New syscall: add to `abi/`, wire kernel handler + libnx wrapper + std
  mapping if applicable, document in `README.md` syscall table, cover with a
  `ktest` case.
- Keep `README.md`, `bench/README.md`, `bench/RESULTS.md` accurate when
  behavior, tables, or numbers change.

## 6. Guardrails (flexible, but be explicit)

- `refs/` is documentation only: read, never compile, never commit changes
  there.
- `tegra_qemu/` is a large external tree: read-only by default. If a fix
  belongs there, explain why nxk side cannot fix it and keep the diff minimal.
- Never assume real Switch hardware: emulator gaps are documented in README
  ("Limites": xHCI firmware, SD power/pins, DSI panel init, GPIO buttons,
  GPU/audio/Wi-Fi/Joy-Con/DVFS). Do not add shipless drivers or power-unsafe
  clock/voltage code.
- Do not commit `build/`, `*/target/`, `~/.cache/nxk/`, SD images, or
  `screendump` PPMs. Do not invent URLs.
- Verify by execution (build + `nxtest.py`) rather than by reasoning. If a
  change cannot be tested in the emulator, state what was and was not covered.

## 7. Commits (mandatory)

- After every task, `commit` **and** `push` the result (do not leave changes
  unpushed in the working tree).
- Every commit message must include a `Co-authored-by` trailer naming the AI
  model actually used, e.g.:
  `Co-authored-by: <model-name> <noreply@example.com>`
  Replace `<model-name>` with the exact model id (e.g. what `opencode models`
  reports). Never omit it, never invent a human author.
