# PS4 layer (port of iss_pc)

`ps4/` is a Rust rewrite of [HayatoG/iss_pc](https://github.com/HayatoG/iss_pc)
(inFAMOUS Second Son, PS4 CUSA00223 1.07 on PC). Upstream is about 330 000
lines of C/C++/Python, GPL-2.0-or-later; `ps4/` is therefore GPL-2.0-or-later
too (see `LICENSE`). No game file is part of the repository: the user supplies
the dump of their own console.

## What upstream does

The game's x86-64 code runs natively on the host CPU, the PS4 system
libraries (libkernel, libc, audio, pad, save data...) are reimplemented as
a runtime, and the GCN GPU is translated to Vulkan (shadPS4 core). None of
the three transfers to nxk as is.

## What nxk needs instead

| Upstream | nxk replacement | Size |
|---|---|---|
| Native x86-64 execution (AVX required) | x86-64 -> AArch64 translator (decoder, then JIT; Cortex-A57 has NEON only, so AVX is emulated) | the main work |
| PS4 runtime in C (`port/src`, ~19 000 lines) | an EL0 service on nxk IPC, files via `fs`, audio via `audio`, input via `input` | large |
| GCN -> SPIR-V -> Vulkan (shadPS4, ~117 000 lines) | GCN -> `nxk-shader` IR -> SASS, draws through `gpu` command streams | very large |
| Python launcher | a library card + `display` provider | small |

Limits that stay whatever the code: about 4 GB of RAM shared by the whole
console (the game targets more), and no dump to test with in the repository.
Two things cannot be checked in the emulator: shader output (it runs no
shaders) and the game itself (no dump).

## Technical choices

CPU execution, options considered:

1. Interpreter. Simple and testable, but far too slow for a 3D game on 4 A57.
2. JIT translator. Fast enough in principle; needs W^X toggling (`mem_protect`)
   and a decoder for the x86-64 subset the game uses. Retained as the target.
3. Ahead-of-time recompilation of the dump on the console. Cannot handle
   self-modifying or JIT-generated code; heavy for 4 GB.

Plan: build a decoder and an interpreter first (differentially tested against
the host x86-64 CPU on the PC, which is the only way to test without the
dump), then put the JIT behind the same decoder.

## Phases

| Phase | Content | State |
|---|---|---|
| 1 | `ps4fmt`: SHA-1, NIDs, `param.sfo`, plaintext ELF/SELF | done (host tests) |
| 2 | `x86emu`: decoder + interpreter, differential tests on the PC | next |
| 3 | PS4 runtime service (libkernel/libc imports by NID, threads, files) | to do |
| 4 | GCN -> SASS translator on top of `shader/` | to do |
| 5 | JIT, library card, tests on the Erista | to do |

Run the host tests: `cd ps4 && cargo test`.
