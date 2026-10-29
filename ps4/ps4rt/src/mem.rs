//! Guest address space: a sorted set of mapped regions backed by host memory.

use x86emu::cpu::Mem;

struct Region {
    base: u64,
    data: Vec<u8>,
}

/// The guest's memory. Regions never overlap; unmapped accesses fail.
#[derive(Default)]
pub struct GuestMem {
    regions: Vec<Region>,
    /// Index of the region hit last (most accesses stay in one region).
    last: usize,
}

impl GuestMem {
    /// Map `size` zeroed bytes at `base`.
    ///
    /// # Returns
    /// `false` when the range wraps or overlaps a mapped region.
    pub fn map(&mut self, base: u64, size: u64) -> bool {
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        if self
            .regions
            .iter()
            .any(|r| base < r.base + r.data.len() as u64 && r.base < end)
        {
            return false;
        }
        let at = self.regions.partition_point(|r| r.base < base);
        self.regions.insert(
            at,
            Region {
                base,
                data: vec![0; size as usize],
            },
        );
        self.last = 0;
        true
    }

    /// The bytes `[addr, addr + len)` when they lie in one region.
    pub fn slice(&mut self, addr: u64, len: usize) -> Option<&mut [u8]> {
        let i = self.find(addr, len as u64)?;
        let r = &mut self.regions[i];
        let o = (addr - r.base) as usize;
        Some(&mut r.data[o..o + len])
    }

    fn find(&mut self, addr: u64, len: u64) -> Option<usize> {
        let inside = |r: &Region| {
            addr >= r.base
                && addr
                    .checked_add(len)
                    .is_some_and(|e| e <= r.base + r.data.len() as u64)
        };
        if self.regions.get(self.last).is_some_and(inside) {
            return Some(self.last);
        }
        let i = self
            .regions
            .partition_point(|r| r.base <= addr)
            .checked_sub(1)?;
        inside(&self.regions[i]).then(|| {
            self.last = i;
            i
        })
    }
}

impl Mem for GuestMem {
    fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let b = self.slice(addr, size as usize)?;
        let mut v = [0u8; 8];
        v[..b.len()].copy_from_slice(b);
        Some(u64::from_le_bytes(v))
    }

    fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        match self.slice(addr, size as usize) {
            Some(b) => {
                b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_read_write() {
        let mut m = GuestMem::default();
        assert!(m.map(0x2000, 0x1000));
        assert!(m.map(0x1000, 0x1000));
        assert!(!m.map(0x1800, 0x100), "overlap refused");
        assert!(m.write(0x1ffc, 4, 0xdead_beef));
        assert_eq!(m.read(0x1ffc, 4), Some(0xdead_beef));
        assert_eq!(m.read(0x1ffe, 4), None, "an access crossing regions faults");
        assert_eq!(m.read(0x3000, 1), None);
        assert!(m.write(0x2000, 8, u64::MAX));
        assert_eq!(m.read(0x2000, 2), Some(0xffff));
    }
}
