//! Differential tests: run the same machine code on the host CPU and in the interpreter,
//! then compare registers, arithmetic flags and the stack. Linux x86-64 hosts only.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::arch::global_asm;
use std::sync::Mutex;
use x86emu::cpu::{Cpu, Mem, AF, CF, FLAG_MASK, OF, ZF};

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

/// Run each case `ROUNDS` times from random states, comparing the flags in `mask`.
fn check_all(cases: &[(&[u8], u64)], seed: u64) {
    let mut rng = Rng(seed);
    for (code, mask) in cases {
        for _ in 0..ROUNDS {
            check(code, *mask, &mut rng);
        }
    }
}

/// Flags defined after a shift by more than one: OF and AF are not.
const SHIFT_FLAGS: u64 = FLAG_MASK & !(OF | AF);

#[test]
fn shifts_and_rotates() {
    let mut cases: Vec<(Vec<u8>, u64)> = Vec::new();
    for op in [0u8, 1, 2, 3, 4, 5, 7] {
        // Rotates leave SF/ZF/PF/AF alone (the harness compares them as untouched).
        let mask = if op < 4 { CF } else { SHIFT_FLAGS };
        let one = if op < 4 { CF | OF } else { FLAG_MASK & !AF };
        for prefix in [&[0x48u8][..], &[], &[0x66], &[0x49]] {
            let modrm = 0xc0 | (op << 3) | 3; // rbx / r11
            let mut by_cl = prefix.to_vec();
            by_cl.extend([0xd3, modrm, 0xc3]);
            cases.push((by_cl, mask));
            let mut by_1 = prefix.to_vec();
            by_1.extend([0xd1, modrm, 0xc3]);
            cases.push((by_1, one));
            for imm in [1u8, 7, 31, 63, 200] {
                let mut c = prefix.to_vec();
                c.extend([0xc1, modrm, imm, 0xc3]);
                cases.push((c, if imm == 1 { one } else { mask }));
            }
        }
        // Byte forms: bl and ah (no REX).
        cases.push((vec![0xd2, 0xc0 | (op << 3) | 3, 0xc3], mask));
        cases.push((vec![0xc0, 0xc0 | (op << 3) | 4, 5, 0xc3], mask));
    }
    let refs: Vec<(&[u8], u64)> = cases.iter().map(|(c, m)| (&c[..], *m)).collect();
    check_all(&refs, 0x5151_e0e0_7a7a_3c3c);
}

#[test]
fn mul_div_and_unary() {
    const MULF: u64 = CF | OF;
    let cases: &[(&[u8], u64)] = &[
        (&[0x48, 0xf7, 0xe3, 0xc3], MULF),                   // mul rbx
        (&[0xf7, 0xe3, 0xc3], MULF),                         // mul ebx
        (&[0xf6, 0xe3, 0xc3], MULF),                         // mul bl
        (&[0x66, 0xf7, 0xe3, 0xc3], MULF),                   // mul bx
        (&[0x48, 0xf7, 0xeb, 0xc3], MULF),                   // imul rbx
        (&[0xf7, 0xeb, 0xc3], MULF),                         // imul ebx
        (&[0x48, 0x0f, 0xaf, 0xc3, 0xc3], MULF),             // imul rax, rbx
        (&[0x0f, 0xaf, 0xcb, 0xc3], MULF),                   // imul ecx, ebx
        (&[0x66, 0x0f, 0xaf, 0xcb, 0xc3], MULF),             // imul cx, bx
        (&[0x48, 0x6b, 0xc3, 0xf9, 0xc3], MULF),             // imul rax, rbx, -7
        (&[0x69, 0xc3, 0x40, 0x42, 0x0f, 0x00, 0xc3], MULF), // imul eax, ebx, 1000000
        // Dividends and divisors kept in range so the native run cannot fault.
        (
            &[0x48, 0x83, 0xcb, 0x01, 0x31, 0xd2, 0x48, 0xf7, 0xf3, 0xc3],
            0,
        ), // or rbx,1; xor edx,edx; div rbx
        (&[0x83, 0xcb, 0x01, 0x31, 0xd2, 0xf7, 0xf3, 0xc3], 0), // div ebx
        (
            &[
                0x48, 0xd1, 0xeb, 0x48, 0x83, 0xcb, 0x01, 0x48, 0x99, 0x48, 0xf7, 0xfb, 0xc3,
            ],
            0,
        ), // idiv rbx
        (&[0xd1, 0xeb, 0x83, 0xcb, 0x01, 0x99, 0xf7, 0xfb, 0xc3], 0), // cdq; idiv ebx
        (&[0x0f, 0xb6, 0xc0, 0x80, 0xcb, 0x80, 0xf6, 0xf3, 0xc3], 0), // movzx eax,al; or bl,0x80; div bl
        (&[0x48, 0xf7, 0xd8, 0xc3], FLAG_MASK),                       // neg rax
        (&[0xf6, 0xdc, 0xc3], FLAG_MASK),                             // neg ah
        (&[0x48, 0xf7, 0xd0, 0xc3], FLAG_MASK),                       // not rax
        (&[0x48, 0xff, 0xc0, 0xc3], FLAG_MASK),                       // inc rax
        (&[0xff, 0xc9, 0xc3], FLAG_MASK),                             // dec ecx
        (&[0xfe, 0xc3, 0xc3], FLAG_MASK),                             // inc bl
        (&[0x66, 0xff, 0x0b, 0xc3], FLAG_MASK),                       // dec word [rbx]
        (&[0x48, 0x98, 0x48, 0x99, 0xc3], FLAG_MASK),                 // cdqe; cqo
        (&[0x98, 0x66, 0x99, 0xc3], FLAG_MASK),                       // cwde; cwd
        (&[0x66, 0x98, 0x99, 0xc3], FLAG_MASK),                       // cbw; cdq
    ];
    check_all(cases, 0x0d1f_0d1f_2468_ace0);
}

#[test]
fn widths_and_extensions() {
    let cases: &[(&[u8], u64)] = &[
        (&[0x88, 0xe3, 0xc3], FLAG_MASK),                   // mov bl, ah
        (&[0x40, 0x88, 0xe3, 0xc3], FLAG_MASK),             // mov bl, spl
        (&[0x66, 0x89, 0xd8, 0xc3], FLAG_MASK),             // mov ax, bx
        (&[0x66, 0x01, 0xd8, 0xc3], FLAG_MASK),             // add ax, bx
        (&[0x00, 0xe3, 0xc3], FLAG_MASK),                   // add bl, ah
        (&[0x28, 0xc4, 0xc3], FLAG_MASK),                   // sub ah, al
        (&[0x80, 0xf3, 0x5a, 0xc3], FLAG_MASK & !AF),       // xor bl, 0x5a
        (&[0x66, 0x81, 0xc3, 0x34, 0x92, 0xc3], FLAG_MASK), // add bx, 0x9234
        (&[0x04, 0x80, 0xc3], FLAG_MASK),                   // add al, 0x80
        (&[0x48, 0x2d, 0x00, 0x00, 0x00, 0x80, 0xc3], FLAG_MASK), // sub rax, imm32
        (&[0xa8, 0x81, 0xc3], FLAG_MASK & !AF),             // test al, 0x81
        (&[0xf6, 0xc7, 0x10, 0xc3], FLAG_MASK & !AF),       // test bh, 0x10
        (&[0x0f, 0xb6, 0xc7, 0xc3], FLAG_MASK),             // movzx eax, bh
        (&[0x48, 0x0f, 0xb7, 0xc3, 0xc3], FLAG_MASK),       // movzx rax, bx
        (&[0x48, 0x0f, 0xbe, 0xc3, 0xc3], FLAG_MASK),       // movsx rax, bl
        (&[0x0f, 0xbf, 0xcb, 0xc3], FLAG_MASK),             // movsx ecx, bx
        (&[0x48, 0x63, 0xc3, 0xc3], FLAG_MASK),             // movsxd rax, ebx
        (&[0x48, 0x63, 0x0b, 0xc3], FLAG_MASK),             // movsxd rcx, [rbx]
        (&[0x0f, 0xb6, 0x0a, 0xc3], FLAG_MASK),             // movzx ecx, byte [rdx]
        (&[0xc6, 0x03, 0x99, 0xc3], FLAG_MASK),             // mov byte [rbx], 0x99
        (&[0x66, 0xc7, 0x03, 0x11, 0x22, 0xc3], FLAG_MASK), // mov word [rbx], 0x2211
        (&[0xb7, 0x42, 0xc3], FLAG_MASK),                   // mov bh, 0x42
        (&[0x41, 0xb4, 0x42, 0xc3], FLAG_MASK),             // mov r12b, 0x42
        (&[0x48, 0x87, 0xd8, 0xc3], FLAG_MASK),             // xchg rax, rbx
        (&[0x87, 0x0b, 0xc3], FLAG_MASK),                   // xchg [rbx], ecx
        (&[0x93, 0xc3], FLAG_MASK),                         // xchg eax, ebx
        (&[0x0f, 0xc8, 0x48, 0x0f, 0xcb, 0xc3], FLAG_MASK), // bswap eax; bswap rbx
    ];
    check_all(cases, 0x7172_7374_7576_7778);
}

#[test]
fn conditions_and_bits() {
    let mut cases: Vec<(Vec<u8>, u64)> = Vec::new();
    for cc in 0..16u8 {
        cases.push((
            vec![0x48, 0x39, 0xd8, 0x0f, 0x90 | cc, 0xc1, 0xc3],
            FLAG_MASK,
        )); // cmp; setcc cl
        cases.push((
            vec![0x48, 0x39, 0xd8, 0x48, 0x0f, 0x40 | cc, 0xcb, 0xc3],
            FLAG_MASK,
        )); // cmovcc rcx, rbx
        cases.push((vec![0x39, 0xd8, 0x0f, 0x40 | cc, 0xcb, 0xc3], FLAG_MASK)); // cmovcc ecx, ebx
    }
    let fixed: &[(&[u8], u64)] = &[
        (&[0x48, 0x0f, 0xbc, 0xc3, 0xc3], ZF), // bsf rax, rbx
        (&[0x0f, 0xbd, 0xc3, 0xc3], ZF),       // bsr eax, ebx
        (&[0xf3, 0x48, 0x0f, 0xbc, 0xc3, 0xc3], CF | ZF), // tzcnt rax, rbx
        (&[0xf3, 0x0f, 0xbd, 0xc3, 0xc3], CF | ZF), // lzcnt eax, ebx
        (&[0xf3, 0x66, 0x0f, 0xbc, 0xc3, 0xc3], CF | ZF), // tzcnt ax, bx
        (&[0xf3, 0x48, 0x0f, 0xb8, 0xc3, 0xc3], FLAG_MASK), // popcnt rax, rbx
        (&[0x48, 0x0f, 0xa3, 0xd8, 0xc3], CF), // bt rax, rbx
        (&[0x48, 0x0f, 0xab, 0xd8, 0xc3], CF), // bts rax, rbx
        (&[0x0f, 0xba, 0xf0, 0x27, 0xc3], CF), // btr eax, 39
        (&[0x48, 0x0f, 0xba, 0xf8, 0x3f, 0xc3], CF), // btc rax, 63
        (&[0x48, 0x83, 0xe0, 0x3f, 0x48, 0x0f, 0xab, 0x03, 0xc3], CF), // and rax,63; bts [rbx], rax
        (&[0x48, 0x0f, 0xb1, 0x0b, 0xc3], FLAG_MASK), // cmpxchg [rbx], rcx
        (&[0x48, 0x8b, 0x03, 0x48, 0x0f, 0xb1, 0x0b, 0xc3], FLAG_MASK), // equal case
        (&[0x0f, 0xb1, 0xcb, 0xc3], FLAG_MASK), // cmpxchg ebx, ecx
        (&[0x48, 0x0f, 0xc1, 0x03, 0xc3], FLAG_MASK), // xadd [rbx], rax
        (&[0xf0, 0x0f, 0xc1, 0x03, 0xc3], FLAG_MASK), // lock xadd [rbx], eax
    ];
    let mut refs: Vec<(&[u8], u64)> = cases.iter().map(|(c, m)| (&c[..], *m)).collect();
    refs.extend_from_slice(fixed);
    check_all(&refs, 0x0c0d_0e0f_1011_1213);
}

#[test]
fn calls_strings_and_stack() {
    let cases: &[(&[u8], u64)] = &[
        // call +1; ret (inner); ret
        (
            &[0xe8, 0x01, 0, 0, 0, 0xc3, 0x48, 0x01, 0xd8, 0xc3],
            FLAG_MASK,
        ),
        // lea rcx,[rip+3]; call rcx; ret; inc eax; ret
        (
            &[
                0x48, 0x8d, 0x0d, 0x03, 0, 0, 0, 0xff, 0xd1, 0xc3, 0xff, 0xc0, 0xc3,
            ],
            FLAG_MASK,
        ),
        // push rbp; mov rbp,rsp; sub rsp,32; mov [rbp-8],rax; leave; ret
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x20, 0x48, 0x89, 0x45, 0xf8, 0xc9, 0xc3,
            ],
            FLAG_MASK,
        ),
        (
            &[0x6a, 0xf0, 0x58, 0x68, 0x00, 0x00, 0x00, 0x80, 0x5b, 0xc3],
            FLAG_MASK,
        ), // push -16; pop rax; push imm32; pop rbx
        (&[0xff, 0x33, 0x8f, 0x02, 0xc3], FLAG_MASK), // push [rbx]; pop [rdx]
        // mov rdi,rbx; mov rsi,rdx; mov ecx,9; cld; rep movsb
        (
            &[
                0x48, 0x89, 0xdf, 0x48, 0x89, 0xd6, 0xb9, 9, 0, 0, 0, 0xfc, 0xf3, 0xa4, 0xc3,
            ],
            FLAG_MASK,
        ),
        // mov rdi,rbx; mov ecx,5; cld; rep stosq
        (
            &[
                0x48, 0x89, 0xdf, 0xb9, 5, 0, 0, 0, 0xfc, 0xf3, 0x48, 0xab, 0xc3,
            ],
            FLAG_MASK,
        ),
        // mov rdi,rbx; mov rsi,rdx; cld; movsd (one element, no rep)
        (
            &[0x48, 0x89, 0xdf, 0x48, 0x89, 0xd6, 0xfc, 0xa5, 0xc3],
            FLAG_MASK,
        ),
        // jump table: lea rcx,[rip+6]; and eax,1; jmp [rcx+rax*8] is replaced by a register jump
        (
            &[
                0x48, 0x8d, 0x0d, 0x04, 0, 0, 0, 0xff, 0xe1, 0x90, 0x90, 0xc3,
            ],
            FLAG_MASK,
        ),
        (
            &[0x0f, 0x1f, 0x44, 0, 0, 0x66, 0x90, 0xc2, 0x00, 0x00],
            FLAG_MASK,
        ), // nops; ret 0
    ];
    check_all(cases, 0x9abc_def0_1234_5678);
}

/// SSE2 integer ops, checked through the GPRs: xmm0 = rbx:rax and xmm2 = rdx:rcx are built
/// with movq + punpcklqdq, the op runs, and xmm0 comes back in rax (low) and rbx (high).
#[test]
fn sse_integer() {
    const SETUP: [u8; 30] = [
        0x66, 0x48, 0x0f, 0x6e, 0xc0, // movq xmm0, rax
        0x66, 0x48, 0x0f, 0x6e, 0xcb, // movq xmm1, rbx
        0x66, 0x0f, 0x6c, 0xc1, // punpcklqdq xmm0, xmm1
        0x66, 0x48, 0x0f, 0x6e, 0xd1, // movq xmm2, rcx
        0x66, 0x48, 0x0f, 0x6e, 0xda, // movq xmm3, rdx
        0x66, 0x0f, 0x6c, 0xd3, // punpcklqdq xmm2, xmm3
        0x90, 0x90,
    ];
    const OUT: [u8; 16] = [
        0x66, 0x48, 0x0f, 0x7e, 0xc0, // movq rax, xmm0
        0x66, 0x0f, 0x73, 0xd8, 0x08, // psrldq xmm0, 8
        0x66, 0x48, 0x0f, 0x7e, 0xc3, // movq rbx, xmm0
        0xc3,
    ];
    let mut ops: Vec<Vec<u8>> = Vec::new();
    for b in [
        0x60u8, 0x61, 0x62, 0x6c, 0x68, 0x69, 0x6a, 0x6d, 0x64, 0x65, 0x66, 0x74, 0x75, 0x76, 0xd4,
        0xfc, 0xfd, 0xfe, 0xf8, 0xf9, 0xfa, 0xfb, 0xf4, 0xdb, 0xdf, 0xeb, 0xef, 0xd1, 0xd2, 0xd3,
        0xe1, 0xe2, 0xf1, 0xf2, 0xf3,
    ] {
        ops.push(vec![0x66, 0x0f, b, 0xc2]); // op xmm0, xmm2
    }
    for b in [0x54u8, 0x55, 0x56, 0x57] {
        ops.push(vec![0x0f, b, 0xc2]); // andps / andnps / orps / xorps
    }
    for imm in [0u8, 1, 3, 7, 8, 15, 16, 31, 33, 63, 64, 200] {
        for (b, reg) in [
            (0x71u8, 2u8),
            (0x71, 4),
            (0x71, 6),
            (0x72, 2),
            (0x72, 4),
            (0x72, 6),
            (0x73, 2),
            (0x73, 6),
            (0x73, 3),
            (0x73, 7),
        ] {
            ops.push(vec![0x66, 0x0f, b, 0xc0 | (reg << 3), imm]);
        }
        ops.push(vec![0x66, 0x0f, 0x70, 0xc2, imm.wrapping_mul(37)]); // pshufd xmm0, xmm2
    }
    // Shift counts small enough to matter: xmm2's low quadword = rcx & 31.
    let small = [0x48u8, 0x83, 0xe1, 0x1f]; // and rcx, 31
    ops.push(vec![0x66, 0x0f, 0x6f, 0xc2]); // movdqa xmm0, xmm2
    ops.push(vec![0xf3, 0x0f, 0x7e, 0xc2]); // movq xmm0, xmm2 (zero-extends)
    ops.push(vec![0x66, 0x0f, 0xd6, 0xd0]); // movq xmm0, xmm2 (store form, register)
    ops.push(vec![0x66, 0x0f, 0x7e, 0xd0, 0x66, 0x0f, 0x6e, 0xc0]); // movd eax, xmm2; movd xmm0, eax
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for op in &ops {
        for pre in [&[][..], &small[..]] {
            let mut c = pre.to_vec();
            c.extend_from_slice(&SETUP);
            c.extend_from_slice(op);
            c.extend_from_slice(&OUT);
            cases.push(c);
        }
    }
    let refs: Vec<(&[u8], u64)> = cases.iter().map(|c| (&c[..], 0)).collect();
    check_all(&refs, 0x55aa_33cc_0ff0_9669);
}

/// Regenerates `apps/x86test/src/vectors.rs`: `cargo test -p x86emu --test differential
/// gen_vectors -- --ignored --nocapture > apps/x86test/src/vectors.rs`.
#[test]
#[ignore]
fn gen_vectors() {
    let _guard = NATIVE.lock().unwrap_or_else(|e| e.into_inner());
    let page = unsafe { mmap(std::ptr::null_mut(), 4096, 7, 0x22, -1, 0) };
    let stack = vec![0u64; 512].into_boxed_slice();
    let s = stack.as_ptr() as u64 + 2048;
    let edges = [
        0u64,
        1,
        u64::MAX,
        1 << 63,
        (1 << 63) - 1,
        0xffff_ffff,
        0x8000_0000,
    ];
    println!("// Generated by gen_vectors (ps4/x86emu/tests/differential.rs) from a native run.");
    println!("// (op, 64-bit, rax, rbx, flags_in, rax_out, rbx_out, flags_out)");
    println!("pub const ALU: &[(u8, bool, u64, u64, u64, u64, u64, u64)] = &[");
    for op in 0..8u8 {
        for w in [true, false] {
            let code = [if w { 0x48 } else { 0x40 }, (op << 3) | 1, 0xd8, 0xc3];
            unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), page, 4) };
            for &a in &edges {
                for &b in &edges {
                    for fin in [0u64, 1] {
                        let mut r = NativeRegs {
                            code: page as u64,
                            flags: 2 | fin,
                            ..Default::default()
                        };
                        r.gpr[0] = a;
                        r.gpr[3] = b;
                        r.gpr[4] = s;
                        unsafe { ps4_native_run(&mut r) };
                        let mask = if matches!(op, 1 | 4 | 6) {
                            FLAG_MASK & !AF
                        } else {
                            FLAG_MASK
                        };
                        println!(
                            "    ({op}, {w}, {a:#x}, {b:#x}, {fin}, {:#x}, {:#x}, {:#x}),",
                            r.gpr[0],
                            r.gpr[3],
                            r.flags & mask
                        );
                    }
                }
            }
        }
    }
    println!("];");
}
