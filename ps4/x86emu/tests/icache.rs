//! Instruction cache: self-modifying code is seen, and the cache pays off.

use x86emu::cpu::{Cpu, Mem};

struct VecMem {
    base: u64,
    data: Vec<u8>,
}

impl Mem for VecMem {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let o = addr.checked_sub(self.base)? as usize;
        let b = self.data.get(o..o + size as usize)?;
        Some(b.iter().rev().fold(0, |v, x| v << 8 | *x as u64))
    }
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        let Some(o) = addr.checked_sub(self.base).map(|o| o as usize) else {
            return false;
        };
        match self.data.get_mut(o..o + size as usize) {
            Some(b) => {
                b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }
}

const BASE: u64 = 0x1000;
const SENTINEL: u64 = 0xdead_0000;

fn machine(code: &[u8]) -> (Cpu, VecMem) {
    let mut data = vec![0u8; 0x2000];
    data[..code.len()].copy_from_slice(code);
    let mut cpu = Cpu::default();
    cpu.rip = BASE;
    cpu.gpr[4] = BASE + 0x1ff8;
    let mut mem = VecMem { base: BASE, data };
    assert!(mem.write(cpu.gpr[4], 8, SENTINEL));
    (cpu, mem)
}

#[test]
fn guest_write_to_cached_code_is_seen() {
    let code = [
        0xb9, 0x07, 0, 0, 0, // 0:  mov ecx, 7        (patched on the second pass)
        0x85, 0xdb, // 5:  test ebx, ebx
        0x75, 0x11, // 7:  jne done
        0xbb, 1, 0, 0, 0, // 9:  mov ebx, 1
        0xc7, 0x05, 0xe8, 0xff, 0xff, 0xff, 0xb9, 0x09, 0, 0, // 14: mov dword [rip-24], 0x9b9
        0xeb, 0xe6, // 24: jmp 0
        0xc3, // 26: done: ret
    ];
    let (mut cpu, mut mem) = machine(&code);
    cpu.run(&mut mem, SENTINEL, 100).unwrap();
    assert_eq!(cpu.gpr[1], 9, "stale decoded instruction was executed");
}

#[test]
fn counts_a_loop() {
    // xor eax,eax; mov ecx,N; l: add rax,rcx; sub ecx,1; jne l; ret
    let n: u32 = 1_000_000;
    let mut code = vec![0x31, 0xc0, 0xb9];
    code.extend_from_slice(&n.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xc8, 0x83, 0xe9, 0x01, 0x75, 0xf8, 0xc3]);
    let (mut cpu, mut mem) = machine(&code);
    let t = std::time::Instant::now();
    cpu.run(&mut mem, SENTINEL, 10_000_000).unwrap();
    let dt = t.elapsed().as_secs_f64();
    assert_eq!(cpu.gpr[0], n as u64 * (n as u64 + 1) / 2);
    eprintln!(
        "interpreter: {:.1} MIPS",
        3.0 * n as f64 / dt / 1e6
    );
}

struct Counting(VecMem, u64);
impl Mem for Counting {
    fn read(&mut self, a: u64, s: u8) -> Option<u64> {
        self.1 += 1;
        self.0.read(a, s)
    }
    fn write(&mut self, a: u64, s: u8, v: u64) -> bool {
        self.0.write(a, s, v)
    }
}

#[test]
fn loop_hits_the_cache() {
    let n: u32 = 1000;
    let mut code = vec![0x31, 0xc0, 0xb9];
    code.extend_from_slice(&n.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xc8, 0x83, 0xe9, 0x01, 0x75, 0xf8, 0xc3]);
    let (mut cpu, mem) = machine(&code);
    let mut mem = Counting(mem, 0);
    cpu.run(&mut mem, SENTINEL, 100_000).unwrap();
    // Each instruction is fetched once (at most 15 byte reads) plus the stack pop.
    assert!(mem.1 < 200, "{} guest reads for {} iterations", mem.1, n);
}
