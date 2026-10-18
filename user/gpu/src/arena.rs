//! First-fit allocation of ranges in a region of GPU-visible memory
//! (the program regions the client's shaders go to).

extern crate alloc;

use alloc::vec::Vec;

pub struct Arena {
    /// Free ranges (start, length), sorted by start.
    free: Vec<(u32, u32)>,
}

impl Arena {
    pub fn new(start: u32, len: u32) -> Self {
        Self {
            free: alloc::vec![(start, len)],
        }
    }

    /// `len` bytes at a multiple of `align` (a power of two), or none.
    pub fn alloc(&mut self, len: u32, align: u32) -> Option<u32> {
        for i in 0..self.free.len() {
            let (s, l) = self.free[i];
            let at = s.next_multiple_of(align);
            let pad = at - s;
            if l >= pad + len {
                let rest = l - pad - len;
                self.free.remove(i);
                if rest > 0 {
                    self.free.insert(i, (at + len, rest));
                }
                if pad > 0 {
                    self.free.insert(i, (s, pad));
                }
                return Some(at);
            }
        }
        None
    }

    pub fn release(&mut self, at: u32, len: u32) {
        let i = self.free.partition_point(|&(s, _)| s < at);
        self.free.insert(i, (at, len));
        // Merge with the neighbours.
        if i + 1 < self.free.len() && self.free[i].0 + self.free[i].1 == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == self.free[i].0 {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fit_and_merge() {
        let mut a = Arena::new(0x1000, 0x1000);
        let x = a.alloc(0x300, 0x100).unwrap();
        let y = a.alloc(0x300, 0x100).unwrap();
        let z = a.alloc(0x300, 0x100).unwrap();
        assert_eq!((x, y, z), (0x1000, 0x1300, 0x1600));
        a.release(y, 0x300);
        assert_eq!(a.alloc(0x400, 0x100), Some(0x1900));
        a.release(x, 0x300);
        a.release(z, 0x300);
        assert_eq!(a.alloc(0x800, 0x100), Some(0x1000));
        assert_eq!(a.alloc(0x10000, 0x100), None);
    }
}
