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
| 2 | `x86emu`: decoder + interpreter, differential tests on the PC | integer set done: ALU, mov/movzx/movsx, lea, xchg, push/pop, call/ret/leave, jmp/jcc (direct and indirect), shifts and rotates, mul/imul/div/idiv, neg/not/inc/dec, setcc/cmovcc, bt*, bsf/bsr/tzcnt/lzcnt/popcnt, bswap, cmpxchg/xadd, rep movs/stos, 8/16/32/64-bit operands with ah..bh, `fs`/`gs`; SSE2 integer and data moves (movdqa/movups/movd/movq, padd/psub/pcmpeq/pcmpgt/pmuludq, logic, shifts, pshufd, punpck). Every form compared with the host CPU (registers, defined flags, memory). Not yet: VEX/AVX, x87, SSE floating point, `syscall` |
| 3 | PS4 runtime (`ps4rt`): loader, NID linker, HLE imports | started: maps PT_LOAD/RELRO, reads the dynamic tables in either layout (SCE tags in `PT_SCE_DYNLIBDATA` with `NID#lib#module` names, or standard `DT_*`), applies RELATIVE / 64 / GLOB_DAT / JUMP_SLOT, binds imports to guest modules first (the game's own `libc.prx`), then to HLE thunks; an import nothing provides stops the run with its NID when called. HLE so far: `write`, `_exit`. The entry is called with the SysV alignment (rsp + 8 on 16 bytes), checked by the interpreter: legacy-SSE 16-byte accesses other than movdqu/movups stop with `Misaligned`, as #GP would. Not yet: TLS relocations, threads, files, the other 1200 imports, data imports served by HLE (an STT_OBJECT import such as `__stack_chk_guard` gets a code-thunk address and faults when read, instead of naming its NID), the real entry convention of the eboot (to take from upstream's runtime) |
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
Erista (`x86test guest-elf`, `guest-sce`; hwboot run 77).

Decoder coverage of a binary: `x86emu/examples/coverage.rs` reads `mnemonic<TAB>bytes`
lines (objdump output) and lists what `decode` rejects. The guests decode completely
(1573 instructions, lengths equal to objdump's). Built for the PS4's CPU
(`-march=btver2`) the same C needs VEX: 47 of 1542 instructions at -O2, 309 of 2293 at
-O3 (vmovdqu/vmovdqa, vpxor, vpand, vpmulld, vpsrld, vzeroupper...). VEX decoding onto
the same operations, with the upper ymm halves, is the next decoder step; upstream's
`port/docs/isa_rewrite.md` lists what else the 1.07 eboot uses (BMI1 1615 sites, LZCNT
164, MOVBE 27, SSE4A 96, F16C 13), with AVX and SSE4.2 throughout.

For the JIT: EL0 cannot clean the data cache, but none is needed for code it writes:
`vm::protect` cleans to the point of unification and invalidates the instruction cache
whenever a page becomes executable (`kernel/src/mm/vm.rs`), so RW then RX through
`mem_protect` is enough. tegra_qemu models no instruction cache, so the Erista is the
only check of that path.

Run the host tests: `cd ps4 && cargo test`. On nxk, `x86test` (apps/, in `build.sh` and on the SD card) replays 1568 ALU vectors recorded from a native run on a PC, checks self-modifying code and times the interpreter: `./tools/nxtest.py -c x86test --sd build/sd.img` (identical in the emulator; its MIPS figure is the emulator's, not the console's). On the Erista (hwboot run 76) all vectors match too, at 10.6 MIPS with the CPU at 998 MHz, about 94 cycles per guest instruction: two orders of magnitude below one 1.6 GHz Jaguar core, so the JIT of phase 5 is required, not an option. The wider decoder costs speed (run 77: 8.2 MIPS on the same loop at 998 MHz, 10.1 at 1224 MHz; the guests run at 5.1 and 6.2 MIPS; 47.5 MIPS on a PC, from 55): accepted, since the JIT replaces the interpreter on the hot path and the interpreter stays as the reference. Regenerate the vectors with the `gen_vectors` test (see its comment).
