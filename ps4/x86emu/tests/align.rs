//! 16/32-byte alignment: legacy SSE memory operands and the aligned moves fault when
//! misaligned (#GP); VEX arithmetic and the unaligned moves do not.

use x86emu::cpu::{Cpu, Mem, Stop};

const BASE: u64 = 0x10000;
const SENTINEL: u64 = 0xdead_0000;

struct Flat(Vec<u8>);

impl Mem for Flat {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let o = addr.checked_sub(BASE)? as usize;
        let b = self.0.get(o..o + size as usize)?;
        Some(b.iter().rev().fold(0, |v, x| (v << 8) | *x as u64))
    }
    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        let Some(o) = addr.checked_sub(BASE).map(|o| o as usize) else {
            return false;
        };
        match self.0.get_mut(o..o + size as usize) {
            Some(b) => {
                b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }
}

/// Run `code` with rbx = BASE + 0x1000 + `off`.
fn run(code: &[u8], off: u64) -> Result<(), Stop> {
    let mut mem = Flat(vec![0; 0x3000]);
    mem.0[..code.len()].copy_from_slice(code);
    mem.0[code.len()] = 0xc3;
    let sp = BASE + 0x2ff8;
    assert!(mem.write(sp, 8, SENTINEL));
    let mut cpu = Cpu::new([0; 16], BASE, 2);
    cpu.gpr[4] = sp;
    cpu.gpr[3] = BASE + 0x1000 + off;
    cpu.run(&mut mem, SENTINEL, 100)
}

#[test]
fn alignment_rules() {
    let paddd = [0x66, 0x0f, 0xfe, 0x03]; // paddd xmm0, [rbx]
    let vpaddd = [0xc5, 0xf9, 0xfe, 0x03]; // vpaddd xmm0, xmm0, [rbx]
    let movdqa = [0x66, 0x0f, 0x6f, 0x03];
    let movdqu = [0xf3, 0x0f, 0x6f, 0x03];
    let vmovdqa_y = [0xc5, 0xfd, 0x6f, 0x03]; // vmovdqa ymm0, [rbx]
    let vmovdqu_y = [0xc5, 0xfe, 0x6f, 0x03];
    for code in [
        &paddd[..],
        &vpaddd,
        &movdqa,
        &movdqu,
        &vmovdqa_y,
        &vmovdqu_y,
    ] {
        assert_eq!(run(code, 0), Ok(()), "{code:02x?} aligned");
    }
    let misaligned = BASE + 0x1008;
    assert_eq!(run(&paddd, 8), Err(Stop::Misaligned(misaligned)));
    assert_eq!(run(&movdqa, 8), Err(Stop::Misaligned(misaligned)));
    assert_eq!(run(&vpaddd, 8), Ok(()));
    assert_eq!(run(&movdqu, 8), Ok(()));
    assert_eq!(run(&vmovdqu_y, 8), Ok(()));
    // A 32-byte move needs 32-byte alignment.
    assert_eq!(run(&vmovdqa_y, 16), Err(Stop::Misaligned(BASE + 0x1010)));
}
