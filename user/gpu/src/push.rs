//! Pushbuffer methods and GPFIFO entries (NVIDIA open-gpu-doc
//! classes/host/clb06f.h, MAXWELL_CHANNEL_GPFIFO_A):
//! `rustc --edition 2021 --test user/gpu/src/push.rs -o push && ./push`.

extern crate alloc;

use alloc::vec::Vec;

/// Subchannels the channel binds its GR classes to (Mesa's numbering).
pub const SUBC_COMPUTE: u32 = 1;
pub const SUBC_2D: u32 = 3;

/// Host methods, executed by the PBDMA on any subchannel.
pub const SET_OBJECT: u32 = 0x0000;
pub const WFI: u32 = 0x0078;
/// WFI scope: every engine class of the channel.
pub const WFI_SCOPE_ALL: u32 = 1;

/// Method header opcodes (SEC_OP).
const INC: u32 = 1;
const IMMD: u32 = 4;

#[derive(Default)]
pub struct Push {
    pub words: Vec<u32>,
}

fn header(op: u32, count: u32, subc: u32, mthd: u32) -> u32 {
    assert!(mthd.is_multiple_of(4) && mthd < 0x4000 && subc < 8 && count < 0x2000);
    op << 29 | count << 16 | subc << 13 | mthd >> 2
}

impl Push {
    /// `data` to consecutive methods from `mthd`.
    pub fn inc(&mut self, subc: u32, mthd: u32, data: &[u32]) {
        self.words.push(header(INC, data.len() as u32, subc, mthd));
        self.words.extend_from_slice(data);
    }

    /// A 13-bit value carried in the header itself.
    pub fn immd(&mut self, subc: u32, mthd: u32, v: u32) {
        assert!(v < 0x2000);
        self.words.push(header(IMMD, v, subc, mthd));
    }
}

/// GPFIFO entry for `words` 32-bit words at GPU address `va`.
pub fn gp_entry(va: u64, words: u32) -> [u32; 2] {
    assert!(va.is_multiple_of(4) && va >> 40 == 0 && words < 1 << 21);
    [va as u32, (va >> 32) as u32 & 0xFF | words << 10]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        let mut p = Push::default();
        p.inc(SUBC_COMPUTE, SET_OBJECT, &[0xB1C0]);
        p.inc(SUBC_COMPUTE, 0x1B00, &[1, 2, 3, 4]);
        p.immd(0, WFI, WFI_SCOPE_ALL);
        assert_eq!(
            p.words,
            [0x2001_2000, 0xB1C0, 0x2004_26C0, 1, 2, 3, 4, 0x8001_001E]
        );
        assert_eq!(gp_entry(0x1_2345_6780, 11), [0x2345_6780, 1 | 11 << 10]);
    }
}
