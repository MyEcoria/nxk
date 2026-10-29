//! Differential tests: run the same machine code on the host CPU and in the interpreter,
//! then compare registers, arithmetic flags and the stack. Linux x86-64 hosts only.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::arch::global_asm;
use std::sync::Mutex;
use x86emu::cpu::{Cpu, Mem, AF, FLAG_MASK};

#[repr(C)]
#[derive(Clone, Default)]
struct NativeRegs {
    gpr: [u64; 16],
    flags: u64,
    code: u64,
}

global_asm!(
    ".text",
    ".global ps4_native_run",
    "ps4_native_run:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "lea rax, [rip + ps4_saved]",
    "mov [rax], rsp",
    "mov [rax + 8], rdi",
    "mov rsp, [rdi + 32]",
    "push qword ptr [rdi + 128]",
    "popfq",
    "lea rax, [rip + ps4_native_ret]",
    "push rax",
    "push qword ptr [rdi + 136]",
    "mov rax, [rdi]",
    "mov rcx, [rdi + 8]",
    "mov rdx, [rdi + 16]",
    "mov rbx, [rdi + 24]",
    "mov rbp, [rdi + 40]",
    "mov rsi, [rdi + 48]",
    "mov r8, [rdi + 64]",
    "mov r9, [rdi + 72]",
    "mov r10, [rdi + 80]",
    "mov r11, [rdi + 88]",
    "mov r12, [rdi + 96]",
    "mov r13, [rdi + 104]",
    "mov r14, [rdi + 112]",
    "mov r15, [rdi + 120]",
    "mov rdi, [rdi + 56]",
    "ret",
    "ps4_native_ret:",
    "xchg qword ptr [rip + ps4_saved + 8], rdi",
    "mov [rdi], rax",
    "mov [rdi + 8], rcx",
    "mov [rdi + 16], rdx",
    "mov [rdi + 24], rbx",
    "mov [rdi + 32], rsp",
    "mov [rdi + 40], rbp",
    "mov [rdi + 48], rsi",
    "mov [rdi + 64], r8",
    "mov [rdi + 72], r9",
    "mov [rdi + 80], r10",
    "mov [rdi + 88], r11",
    "mov [rdi + 96], r12",
    "mov [rdi + 104], r13",
    "mov [rdi + 112], r14",
    "mov [rdi + 120], r15",
    "pushfq",
    "pop qword ptr [rdi + 128]",
    "lea rax, [rip + ps4_saved]",
    "mov rax, [rax + 8]",
    "mov [rdi + 56], rax",
    "mov rsp, [rip + ps4_saved]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
    ".data",
    "ps4_saved: .quad 0, 0",
    ".text",
);

extern "C" {
    fn ps4_native_run(regs: *mut NativeRegs);
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

/// The trampoline keeps its state in a global, so native runs are serialized.
static NATIVE: Mutex<()> = Mutex::new(());

/// Guest memory backed by host memory: code page and stack.
struct HostMem {
    ranges: Vec<(u64, u64)>,
}

impl Mem for HostMem {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        self.ranges
            .iter()
            .any(|&(a, z)| addr >= a && addr + size as u64 <= z)
            .then(|| {
                let mut v = 0u64;
                for i in 0..size as u64 {
                    v |= (unsafe { *((addr + i) as *const u8) } as u64) << (8 * i);
                }
                v
            })
    }
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        if !self
            .ranges
            .iter()
            .any(|&(a, z)| addr >= a && addr + size as u64 <= z)
        {
            return false;
        }
        for i in 0..size as u64 {
            unsafe { *((addr + i) as *mut u8) = (value >> (8 * i)) as u8 };
        }
        true
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Mostly edge-heavy values, to hit carries, overflows and signs.
    fn value(&mut self) -> u64 {
        match self.next() % 6 {
            0 => 0,
            1 => u64::MAX,
            2 => 1 << 63,
            3 => (1 << 63) - 1,
            4 => self.next() & 0xffff_ffff,
            _ => self.next(),
        }
    }
}

const STACK: usize = 8192;

/// Run `code` (ending in `ret`) natively and interpreted from the same random state.
fn check(code: &[u8], flag_mask: u64, rng: &mut Rng) {
    let _guard = NATIVE.lock().unwrap_or_else(|e| e.into_inner());
    let page = unsafe { mmap(std::ptr::null_mut(), 4096, 7, 0x22, -1, 0) };
    assert!(page as isize != -1);
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), page, code.len()) };
    let stack = vec![0u64; STACK / 8].into_boxed_slice();
    let base = stack.as_ptr() as u64;
    let s = base + (STACK as u64) / 2;

    let mut regs = NativeRegs::default();
    for g in regs.gpr.iter_mut() {
        *g = rng.value();
    }
    regs.gpr[4] = s;
    // Pointer registers get valid addresses, so memory forms hit the stack buffer.
    for r in [1usize, 2, 3, 5, 6, 12] {
        regs.gpr[r] = s - 256 + (rng.next() % 64) * 2;
    }
    regs.flags = 2 | (rng.next() & FLAG_MASK);
    regs.code = page as u64;
    let mut init = vec![0u8; STACK];
    for b in init.iter_mut() {
        *b = rng.next() as u8;
    }
    let restore = |init: &[u8]| unsafe {
        std::ptr::copy_nonoverlapping(init.as_ptr(), base as *mut u8, STACK)
    };

    // Native.
    if std::env::var("X86_TRACE").is_ok() {
        eprintln!(
            "code {:02x?} init {:x?} flags {:#x}",
            code, regs.gpr, regs.flags
        );
    }
    restore(&init);
    let mut native = regs.clone();
    unsafe { ps4_native_run(&mut native) };
    let native_stack: Vec<u8> =
        unsafe { std::slice::from_raw_parts(base as *const u8, STACK).to_vec() };

    // Interpreted: same stack with the sentinel where the trampoline pushes its return address.
    restore(&init);
    const SENTINEL: u64 = 0xdead_beef_0000_0001;
    let mut mem = HostMem {
        ranges: vec![
            (page as u64, page as u64 + 4096),
            (base, base + STACK as u64),
        ],
    };
    assert!(mem.write(s - 8, 8, SENTINEL));
    let mut cpu = Cpu::new(regs.gpr, page as u64, regs.flags);
    cpu.gpr[4] = s - 8;
    let run = cpu.run(&mut mem, SENTINEL, 10_000);
    let emu_stack: Vec<u8> =
        unsafe { std::slice::from_raw_parts(base as *const u8, STACK).to_vec() };

    let hex: String = code.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(run, Ok(()), "code {hex}");
    assert_eq!(cpu.gpr, native.gpr, "gpr, code {hex}, init {:x?}", regs.gpr);
    assert_eq!(
        cpu.flags & flag_mask,
        native.flags & flag_mask,
        "flags, code {hex}, init {:#x}",
        regs.flags
    );
    // The trampoline's own slots (return address, code pointer) differ by design.
    let slot = (STACK / 2 - 16)..(STACK / 2);
    for i in 0..STACK {
        if !slot.contains(&i) {
            assert_eq!(emu_stack[i], native_stack[i], "stack byte {i}, code {hex}");
        }
    }
}

const ROUNDS: usize = 40;

#[test]
fn alu_register_forms() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for op in 0..8u8 {
        for rex_w in [0x48u8, 0x40] {
            for dst in (0..16u8).filter(|r| *r != 4) {
                for src in (0..16u8).filter(|r| *r != 4).step_by(3) {
                    // op r/m, r (01 pattern) and op r, r/m (03 pattern)
                    for lo in [1u8, 3] {
                        let (reg, rm) = if lo == 1 { (src, dst) } else { (dst, src) };
                        let rex = rex_w | ((reg >> 3) << 2) | (rm >> 3);
                        let code = [
                            rex,
                            (op << 3) | lo,
                            0xc0 | ((reg & 7) << 3) | (rm & 7),
                            0xc3,
                        ];
                        // Logic ops leave AF undefined.
                        let mask = if matches!(op, 1 | 4 | 6) {
                            FLAG_MASK & !AF
                        } else {
                            FLAG_MASK
                        };
                        for _ in 0..3 {
                            check(&code, mask, &mut rng);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn alu_immediates() {
    let mut rng = Rng(0x0bad_cafe_f00d_1111);
    for op in 0..8u8 {
        for rex in [0x48u8, 0x49, 0x40] {
            for rm in [0u8, 1, 3, 7] {
                let mask = if matches!(op, 1 | 4 | 6) {
                    FLAG_MASK & !AF
                } else {
                    FLAG_MASK
                };
                for imm in [0u8, 1, 0x7f, 0x80, 0xff] {
                    let c8 = [rex, 0x83, 0xc0 | (op << 3) | rm, imm, 0xc3];
                    let c32 = [
                        rex,
                        0x81,
                        0xc0 | (op << 3) | rm,
                        imm,
                        0x80,
                        0x00,
                        0x80,
                        0xc3,
                    ];
                    for _ in 0..ROUNDS / 8 {
                        check(&c8, mask, &mut rng);
                        check(&c32, mask, &mut rng);
                    }
                }
            }
        }
    }
}

#[test]
fn mov_lea_test() {
    let mut rng = Rng(0x5555_aaaa_1357_9bdf);
    let cases: &[&[u8]] = &[
        &[0x48, 0x89, 0xd8, 0xc3],                               // mov rax, rbx
        &[0x89, 0xd8, 0xc3],                                     // mov eax, ebx (zero-extends)
        &[0x48, 0xb8, 1, 2, 3, 4, 5, 6, 7, 8, 0xc3],             // movabs rax, imm64
        &[0xb9, 0xff, 0xff, 0xff, 0xff, 0xc3],                   // mov ecx, -1
        &[0x48, 0xc7, 0xc0, 0xfe, 0xff, 0xff, 0xff, 0xc3],       // mov rax, -2
        &[0x48, 0x8d, 0x04, 0x8b, 0xc3],                         // lea rax, [rbx+rcx*4]
        &[0x8d, 0x44, 0x4b, 0x10, 0xc3],                         // lea eax, [rbx+rcx*2+16]
        &[0x4c, 0x8d, 0x64, 0x24, 0xf0, 0xc3],                   // lea r12, [rsp-16]
        &[0x48, 0x89, 0x44, 0x24, 0xe0, 0xc3],                   // mov [rsp-32], rax
        &[0x8b, 0x4c, 0x24, 0xe8, 0xc3],                         // mov ecx, [rsp-24]
        &[0x48, 0x8b, 0x13, 0xc3],                               // mov rdx, [rbx]
        &[0xc7, 0x44, 0x24, 0xe0, 0x78, 0x56, 0x34, 0x12, 0xc3], // mov dword [rsp-32], imm32
        &[0x48, 0x85, 0xc3, 0xc3],                               // test rbx, rax
        &[0x85, 0xc9, 0xc3],                                     // test ecx, ecx
        &[0x48, 0x01, 0x03, 0xc3],                               // add [rbx], rax
        &[0x48, 0x03, 0x0a, 0xc3],                               // add rcx, [rdx]
    ];
    for code in cases {
        for _ in 0..ROUNDS {
            check(code, FLAG_MASK & !AF, &mut rng);
        }
    }
}

#[test]
fn stack_and_branches() {
    let mut rng = Rng(0x7777_1111_2222_3333);
    let cases: &[&[u8]] = &[
        &[0x50, 0x5b, 0xc3],                                     // push rax; pop rbx
        &[0x41, 0x54, 0x41, 0x5d, 0xc3],                         // push r12; pop r13
        &[0x48, 0x39, 0xd8, 0x7c, 0x05, 0xb9, 1, 0, 0, 0, 0xc3], // cmp rax,rbx; jl +5; mov ecx,1
        &[
            0x48, 0x39, 0xd8, 0x0f, 0x8f, 0x05, 0, 0, 0, 0xb9, 2, 0, 0, 0, 0xc3,
        ], // cmp; jg rel32
        &[0x48, 0x39, 0xd8, 0x76, 0x05, 0xb9, 3, 0, 0, 0, 0xc3], // jbe
        &[0x48, 0x39, 0xd8, 0x72, 0x05, 0xb9, 4, 0, 0, 0, 0xc3], // jb
        &[0x48, 0x85, 0xc0, 0x74, 0x05, 0xb9, 5, 0, 0, 0, 0xc3], // test; je
        &[0x48, 0x85, 0xc0, 0x7a, 0x05, 0xb9, 6, 0, 0, 0, 0xc3], // test; jp
        &[0xeb, 0x05, 0xb9, 7, 0, 0, 0, 0x90, 0xc3],             // jmp over mov
        &[0xe9, 0x05, 0, 0, 0, 0xb9, 8, 0, 0, 0, 0xc3],          // jmp rel32
    ];
    for code in cases {
        for _ in 0..ROUNDS {
            check(code, FLAG_MASK & !AF, &mut rng);
        }
    }
}
