//! Compute launch descriptor, QMD v01_07 (NVIDIA open-gpu-doc
//! classes/compute/cla1c0qmd.h), the version Mesa's NAK uses from
//! KEPLER_COMPUTE_B on, so for MAXWELL_COMPUTE_B:
//! `rustc --edition 2021 --test user/gpu/src/qmd.rs -o qmd && ./qmd`.

pub const SIZE: usize = 256;

pub struct Qmd(pub [u32; 64]);

/// A launch: program offset in the program region, grid and block in
/// x only, constant buffer 0.
pub struct Launch {
    pub program: u32,
    pub regs: u32,
    pub grid: u32,
    pub block: u32,
    pub cb0: u64,
    pub cb0_size: u32,
}

/// A launch of a client's compute shader: grid and block in three
/// dimensions, constant buffers 0, 1, ... (address, bytes).
pub struct Launch3<'a> {
    pub program: u32,
    pub regs: u32,
    pub grid: [u32; 3],
    pub block: [u32; 3],
    pub cbs: &'a [(u64, u32)],
    /// Bytes of local memory per thread (a multiple of 16).
    pub local_bytes: u32,
}

impl Qmd {
    /// Field at bits `lo..=hi` of the descriptor (`MW(hi:lo)`).
    fn set(&mut self, hi: u32, lo: u32, v: u64) {
        for bit in lo..=hi {
            let (w, b) = ((bit / 32) as usize, bit % 32);
            let one = (v >> (bit - lo)) & 1;
            self.0[w] = self.0[w] & !(1 << b) | (one as u32) << b;
        }
    }

    pub fn new(l: &Launch) -> Self {
        Self::new3(&Launch3 {
            program: l.program,
            regs: l.regs,
            grid: [l.grid, 1, 1],
            block: [l.block, 1, 1],
            cbs: &[(l.cb0, l.cb0_size)],
            local_bytes: 0,
        })
    }

    pub fn new3(l: &Launch3) -> Self {
        let mut q = Self([0; 64]);
        // All six INVALIDATE_*_CACHE bits: nothing stale from an earlier launch.
        q.set(255, 250, 0x3F);
        q.set(287, 256, l.program as u64);
        q.set(366, 366, 1); // RELEASE_MEMBAR_TYPE: FE_SYSMEMBAR
        q.set(369, 368, 1); // CWD_MEMBAR_TYPE: L1_SYSMEMBAR
        q.set(378, 378, 1); // API_VISIBLE_CALL_LIMIT: NO_CHECK
        q.set(415, 384, l.grid[0] as u64);
        q.set(431, 416, l.grid[1] as u64);
        q.set(447, 432, l.grid[2] as u64);
        q.set(579, 576, 7); // QMD_VERSION
        q.set(583, 580, 1); // QMD_MAJOR_VERSION
        q.set(607, 592, l.block[0] as u64);
        q.set(623, 608, l.block[1] as u64);
        q.set(639, 624, l.block[2] as u64);
        q.set(671, 669, 1); // L1_CONFIGURATION: 16 KiB shared
                            // Eight constant buffers of 64 bits each from bit 928: address
                            // low, high (8 bits), size; valid flags from bit 640.
        for (i, &(va, size)) in l.cbs.iter().enumerate().take(8) {
            let o = 64 * i as u32;
            q.set(640 + i as u32, 640 + i as u32, 1); // CONSTANT_BUFFER_VALID
            q.set(959 + o, 928 + o, va & 0xFFFF_FFFF);
            q.set(967 + o, 960 + o, va >> 32);
            q.set(991 + o, 975 + o, size as u64);
        }
        q.set(1463, 1440, l.local_bytes as u64); // SHADER_LOCAL_MEMORY_LOW_SIZE
        q.set(1503, 1496, l.regs as u64);
        // SHADER_LOCAL_MEMORY_CRS_SIZE: where the warp's reconvergence stack
        // spills past its on-chip entries (deep SSY/PBK nesting).
        q.set(1527, 1504, crate::slm::CRS_BYTES_PER_WARP as u64);
        q.set(1535, 1528, 0x30); // SASS_VERSION
        q
    }

    pub fn bytes(&self) -> [u8; SIZE] {
        let mut b = [0; SIZE];
        for (i, w) in self.0.iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let q = Qmd::new(&Launch {
            program: 0x300,
            regs: 16,
            grid: 5,
            block: 128,
            cb0: 0x1_2345_6700,
            cb0_size: 0x100,
        });
        let w = &q.0;
        assert_eq!(w[7] >> 26, 0x3F);
        assert_eq!(w[8], 0x300);
        assert_eq!(w[12], 5);
        assert_eq!(w[13], 1 | 1 << 16);
        assert_eq!(w[18] & 0xFF, 1 << 4 | 7);
        assert_eq!(w[18] >> 16, 128);
        assert_eq!(w[19], 1 | 1 << 16);
        assert_eq!(w[20], 1 | 1 << 29);
        assert_eq!(w[29], 0x2345_6700);
        assert_eq!(w[30], 1 | 0x100 << 15);
        assert_eq!(w[46] >> 24, 16);
        assert_eq!(w[47] >> 24, 0x30);
        assert_eq!(
            w[11] & (1 << 14 | 1 << 16 | 1 << 26),
            1 << 14 | 1 << 16 | 1 << 26
        );
        assert_eq!(q.bytes()[32..36], 0x300u32.to_le_bytes());
    }

    #[test]
    fn three_dimensions_and_buffers() {
        let q = Qmd::new3(&Launch3 {
            program: 0x400,
            regs: 24,
            grid: [3, 4, 5],
            block: [8, 4, 2],
            cbs: &[
                (0x1_0000_0100, 0x40),
                (0x2_0000_0200, 0x80),
                (0x3_0000_0300, 0x100),
            ],
            local_bytes: 64,
        });
        let w = &q.0;
        assert_eq!(w[45] & 0xFF_FFFF, 64);
        assert_eq!((w[12], w[13]), (3, 4 | 5 << 16));
        assert_eq!(w[18] & 0xFFFF_0000, 8 << 16);
        assert_eq!(w[19], 4 | 2 << 16);
        // Constant buffers 0-2 valid.
        assert_eq!(w[20] & 7, 7);
        // Buffer 1: address at bit 992, size at 1039.
        assert_eq!(w[31], 0x2_0000_0200u64 as u32);
        assert_eq!(w[32] & 0xFF, 2);
        assert_eq!(w[32] >> 15 & 0x1_FFFF, 0x80);
        assert_eq!(w[33], 0x3_0000_0300u64 as u32);
        assert_eq!(w[34] & 0xFF, 3);
    }
}
