//! Lecture de l'image de demarrage (reservee a init, qui la recoit projetee).

use nxk_abi::bootfs::{Entry, Header, MAGIC, NAME_LEN};

pub struct Bootfs {
    data: &'static [u8],
}

impl Bootfs {
    /// # Safety
    /// `base` doit designer une image projetee de `len` octets.
    pub unsafe fn new(base: u64, len: u64) -> Option<Self> {
        let data = unsafe { core::slice::from_raw_parts(base as *const u8, len as usize) };
        let fs = Self { data };
        (fs.header()?.magic == MAGIC).then_some(fs)
    }

    fn header(&self) -> Option<Header> {
        if self.data.len() < core::mem::size_of::<Header>() {
            return None;
        }
        Some(unsafe { core::ptr::read_unaligned(self.data.as_ptr() as *const Header) })
    }

    pub fn count(&self) -> usize {
        self.header().map(|h| h.count as usize).unwrap_or(0)
    }

    fn entry(&self, k: usize) -> Entry {
        let off = core::mem::size_of::<Header>() + k * core::mem::size_of::<Entry>();
        unsafe { core::ptr::read_unaligned(self.data.as_ptr().add(off) as *const Entry) }
    }

    pub fn name(&self, k: usize) -> &'static str {
        let e = self.entry(k);
        let off = core::mem::size_of::<Header>() + k * core::mem::size_of::<Entry>();
        let len = e.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&self.data[off..off + len]).unwrap_or("?")
    }

    pub fn find(&self, name: &str) -> Option<&'static [u8]> {
        (0..self.count()).find(|&k| self.name(k) == name).and_then(|k| {
            let e = self.entry(k);
            self.data.get(e.offset as usize..(e.offset + e.size) as usize)
        })
    }
}
