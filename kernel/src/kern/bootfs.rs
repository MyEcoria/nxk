//! Boot image embedded in the kernel, mapped read-only into init.

use nxk_abi::bootfs::{Entry, Header, MAGIC, NAME_LEN};

#[repr(C, align(4096))]
struct Aligned<T: ?Sized>(T);

static IMAGE: &Aligned<[u8]> = &Aligned(*include_bytes!("../../../build/bootfs.img"));

pub fn image() -> &'static [u8] {
    &IMAGE.0
}

fn header() -> Option<Header> {
    let img = image();
    if img.len() < core::mem::size_of::<Header>() {
        return None;
    }
    let h = unsafe { core::ptr::read_unaligned(img.as_ptr() as *const Header) };
    (h.magic == MAGIC).then_some(h)
}

pub fn entry(k: usize) -> Option<Entry> {
    let h = header()?;
    if k >= h.count as usize {
        return None;
    }
    let off = core::mem::size_of::<Header>() + k * core::mem::size_of::<Entry>();
    let e = unsafe { core::ptr::read_unaligned(image().as_ptr().add(off) as *const Entry) };
    Some(e)
}

pub fn find(name: &str) -> Option<&'static [u8]> {
    let h = header()?;
    for k in 0..h.count as usize {
        let e = entry(k)?;
        let len = e.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        if &e.name[..len] == name.as_bytes() {
            let start = e.offset as usize;
            let end = start + e.size as usize;
            return image().get(start..end);
        }
    }
    None
}

pub fn count() -> usize {
    header().map(|h| h.count as usize).unwrap_or(0)
}
