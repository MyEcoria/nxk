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

## CPU, GPU and memory data path

The GPU cannot run the game's x86 code; it runs what the game gives it:
shaders (GCN translated to the `shader/` IR, compiled to SASS by `gpu`),
texture conversion and, later, compute. The aim is that no byte is copied
more than once between the CPU and the GPU, and that the CPU never waits for
the GPU except at a frame boundary.

Facts checked in the tree:

- `shm_create` has an `UNCACHED` flag (Normal-NC, MAIR index 2): the GPU does
  not snoop CPU caches, and Normal-NC lets the CPU combine writes.
  The `gpu` service maps client memory page by page through its GMMU
  (`shm_paddr` per page), so a buffer does not need to be physically contiguous.
- The kernel runs with MAIR = device, Normal write-back, Normal non-cacheable.
  No cache-maintenance syscall exists, and EL0 has no `DC CVAC` (SCTLR_EL1.UCI is not set).
- upstream's `sceKernelAllocateDirectMemory` carries a memory-type argument
  (`memory_type` in `runtime_memory.c`); its values are to be taken from there
  when the runtime is ported, not written from memory.

Options for the guest's memory:

1. *All guest memory in `UNCACHED` shm.* Zero copy, but the guest stack and heap
   would be uncached: the interpreter and the JIT would crawl. Rejected.
2. *Cached memory, copy to the GPU on use, track dirty pages by write faults*
   (upstream's PC approach). Works with no kernel change, but faults and
   copies over a shared LPDDR4 are the most expensive thing available.
   Kept only as the fallback.
3. *Split by PS4 memory type.* GPU-optimised, write-combined memory (what the
   PS4 CPU only writes) is allocated in `UNCACHED` shm and handed to the GPU as is:
   textures, vertex and index buffers, command buffers, render targets. CPU-coherent
   memory stays cached; the GPU sees it through a copy at submission of the ranges a
   draw references, until a cache-clean syscall exists (a later phase, with the usual
   ABI, kernel, libnx and `ktest` work). Retained.

Submission: PM4 command buffers are translated on the CPU into one `Frame` per
image, so one IPC per frame and never one per draw. Texture detiling (GCN tiling to
Tegra block-linear) runs as a compute shader on the GPU, reading the guest memory in
place, so it costs neither CPU time nor a copy.

## Phases

| Phase | Content | State |
|---|---|---|
| 1 | `ps4fmt`: SHA-1, NIDs, `param.sfo`, plaintext ELF/SELF | done (host tests) |
| 2 | `x86emu`: decoder + interpreter, differential tests on the PC | integer set done: ALU, mov/movzx/movsx, lea, xchg, push/pop, call/ret/leave, jmp/jcc (direct and indirect), shifts and rotates, mul/imul/div/idiv, neg/not/inc/dec, setcc/cmovcc, bt*, bsf/bsr/tzcnt/lzcnt/popcnt, bswap, cmpxchg/xadd, rep movs/stos, 8/16/32/64-bit operands with ah..bh, `fs`/`gs`; MOVBE, CRC32 (SSE4.2), BMI1 (andn, bextr, blsi, blsmsk, blsr); SSE2..SSE4.1 integer and data moves (saturating, min/max, multiplies, pack/unpack, pshufb, palignr, pmovsx/zx, ptest, pextr/pinsr, pmovmskb...), legacy and VEX encoded; AVX1: VEX.128 for all of them (upper ymm half cleared), VEX.256 for the moves, the ps/pd logic, vinsertf128/vextractf128, vzeroupper/vzeroall. Alignment faults as on the CPU (legacy 128-bit operands, aligned moves). Every form compared with the host CPU (registers, defined flags, memory, both ymm halves). SSE / AVX floating point on a soft-float core (see below): arithmetic, compares, conversions, rounding, horizontal ops, dpps, blends, shuffles, permutes, broadcasts, F16C, MXCSR. Not yet: x87, SSE4A, SSE4.2 string compares, vtestps/vmaskmovps, `syscall` |
| 3 | PS4 runtime (`ps4rt`): loader, NID linker, HLE imports | started: maps PT_LOAD/RELRO, reads the dynamic tables in either layout (SCE tags in `PT_SCE_DYNLIBDATA` with `NID#lib#module` names, or standard `DT_*`), applies RELATIVE / 64 / GLOB_DAT / JUMP_SLOT, binds imports to guest modules first (the game's own `libc.prx`), then to HLE thunks; an import nothing provides stops the run with its NID when called. HLE so far: `write`, `_exit`. The entry is called with the SysV alignment (rsp + 8 on 16 bytes), checked by the interpreter: legacy-SSE 16-byte accesses other than movdqu/movups stop with `Misaligned`, as #GP would. Not yet: TLS relocations, threads, files, the other 1200 imports, data imports served by HLE (an STT_OBJECT import such as `__stack_chk_guard` gets a code-thunk address and faults when read, instead of naming its NID), the real entry convention of the eboot (to take from upstream's runtime), the floating-point state of threads: a new thread starts with its creator's MXCSR and x87 control word, as FreeBSD's libthr does (inFAMOUS sets FTZ in one thread, then creates its job workers from it; upstream's `runtime_thread.c`), and the main thread's initial values are to be taken from upstream's runtime, not assumed to be 0x1f80 |
| 4 | GCN -> SASS translator on top of `shader/` | to do |
| 5 | JIT, library card, tests on the Erista | to do |

### Test guests

Without a game dump, `ps4/guest/` stands in for the game: a guest "libc" module
(`libmini.c`) and a main program (`main.c`: sorting through function pointers, CRC,
recursion, jump tables, struct copies, 8/16-bit and 128-bit arithmetic, bit tricks,
pointer tables) compiled by gcc at -O2 as x86-64 shared objects, flags chosen as the
PS4 toolchain builds (no CET, no stack protector through `fs:0x28`, `-z now`).
`ps4/guest/build.py` rebuilds them, records the stdout of the same C built natively
(`expected.txt`), and repacks `main.so` into the PS4 layout (`main.sce`: ET_SCE_DYNEXEC,
SCE tags, imports named `NID#lib#module` from libkernel and libc). The outputs are
committed so that neither `build.sh` nor CI needs gcc. Both layouts print exactly what
the native run prints, on the PC (`cargo test -p ps4rt`), in the emulator and on the
Erista (`x86test guest-elf`, `guest-sce`; hwboot run 77). A third pair is the same C at
-O3 for the PS4's CPU (`-march=btver2`: AVX, BMI1, SSE4.x; `*-jaguar.so`), against the
same reference (`x86test guest-jaguar`; run 78).

Decoder coverage of a binary: `x86emu/examples/coverage.rs` reads `mnemonic<TAB>bytes`
lines (objdump output) and lists what `decode` rejects. Every guest decodes completely,
lengths equal to objdump's: 1573 instructions generic, 1542 (-O2) and 2293 (-O3) for
`-march=btver2` (before VEX decoding: 47 and 309 rejected).

The scope follows Jaguar, not the PC: AVX1 only, so a 256-bit integer op (AVX2) is
refused, as it would be #UD on a PS4; BMI1 and MOVBE are in (upstream's
`port/docs/isa_rewrite.md` counts 1615 BMI1 and 27 MOVBE sites in the 1.07 eboot), BMI2
and FMA are not Jaguar's. Left for later: SSE4A (96 sites; no Intel host has it, so it
cannot be compared with a native run), F16C (13 sites) with the floating-point phase. Upper
ymm halves are a second array (`ymm_hi`) next to `xmm` rather than `[[u128; 2]; 16]`, so
the SSE paths stay as they were; legacy writes keep the upper half, VEX.128 writes clear
it.

### Floating point

Results must be the PS4's bit for bit: MXCSR rounding modes and sticky flags, x86's NaN
choice (first source, negative "indefinite" QNaN for invalid operations), FTZ and DAZ.
inFAMOUS turns FTZ on in its job threads (upstream `runtime_thread.c`: without it,
denormals reach divisions in the particle and light code), so FTZ has to be exact.

Options considered:

1. *Host FPU with fix-ups* (NaN choice, FTZ, DAZ in software around native ops; other
   rounding modes and the flags through error-free transformations). Rejected: FTZ
   cannot be patched after the host has rounded with bounded exponents (just below
   2^-126 the host gives the smallest normal where x86 gives zero), and `core` has no
   `sqrt` or `mul_add` in `no_std`.
2. *Host FPU for round-to-nearest, soft-float otherwise.* Rejected for now: it is option
   3 plus a fast path, and needs option 3 as its reference anyway.
3. *Soft-float* (`x86emu/src/fpu.rs`). Retained: one integer-only core generic over
   (exponent, fraction) widths for binary16/32/64, exact by construction, with the
   flags and modes, and the reference a later JIT fast path is checked against.

The core matches the host's SSE unit on every pair of edge and random operands under
the 4 rounding modes x FTZ x DAZ, results and flags (`tests/softfloat.rs`). The tests
settled what the SDM leaves to read between the lines: tininess is detected after
rounding (the band test fails with before-rounding detection), DE is not raised by
cvt*2si or roundss, and is hidden by IE or ZE. Each lane gets its own flags, or-ed into
MXCSR; an unmasked exception stops before the destination is written.

Known gaps: rcpps / rsqrtps are approximations whose exact bits are model-specific
(Jaguar's tables are not Intel's); they are only checked against the SDM's bound. dpps
picks between two different NaNs differently on this host's own P- and E-cores (Core
Ultra 7 155H, measured with the test pinned to each kind of core): the E-core order is
modelled, Jaguar's is unknown. AMD's MXCSR.MM bit is refused (Intel's MXCSR_MASK).

The float guest (`fpmain.c`: series, 4x4 matrices, normalisation, conversions,
specials, FTZ/DAZ as inFAMOUS sets them, MXCSR rounding, a particle loop) prints the
native run's bits, generic and for Jaguar, on the PC, in the emulator and on the Erista
(run 79). The host suites (`cd ps4 && cargo test`) are not in CI yet, so every float
result so far is from Intel cores; none from an AMD one. Next: x87 (80-bit, on the same core with an explicit integer bit).

For the JIT: EL0 cannot clean the data cache, but none is needed for code it writes:
`vm::protect` cleans to the point of unification and invalidates the instruction cache
whenever a page becomes executable (`kernel/src/mm/vm.rs`), so RW then RX through
`mem_protect` is enough. tegra_qemu models no instruction cache, so the Erista is the
only check of that path.

Run the host tests: `cd ps4 && cargo test`. On nxk, `x86test` (apps/, in `build.sh` and on the SD card) replays 1568 ALU vectors recorded from a native run on a PC, checks self-modifying code and times the interpreter: `./tools/nxtest.py -c x86test --sd build/sd.img` (identical in the emulator; its MIPS figure is the emulator's, not the console's). On the Erista (hwboot run 76) all vectors match too, at 10.6 MIPS with the CPU at 998 MHz, about 94 cycles per guest instruction: two orders of magnitude below one 1.6 GHz Jaguar core, so the JIT of phase 5 is required, not an option. The wider decoder first cost speed (run 77: 8.2 MIPS on the same loop at 998 MHz; 47.5 on a PC, from 55); with the rare paths (SIMD, BMI, mul/div, bit ops, strings) kept out of the dispatch loop (`#[inline(never)]`), run 78 measures 9.0 MIPS at 998 MHz, the guests 5.1 to 5.5 MIPS, and 66 MIPS on a PC. The JIT stays required. Regenerate the vectors with the `gen_vectors` test (see its comment).
