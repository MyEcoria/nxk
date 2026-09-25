//! GPU register window (BAR0) and the uncached DMA memory it reads.

use libnx::abi::err::{EIO, ETIMEDOUT};
use libnx::abi::shm::{CONTIGUOUS, UNCACHED};
use libnx::drv::Mmio;
use libnx::mem::SharedMemory;
use libnx::time;

/// Every error of the power-up path: an errno (positive) and what it
/// was doing.
pub type Result<T> = core::result::Result<T, (i64, &'static str)>;

/// Order CPU writes to uncached memory before a register write that
/// makes the GPU read them (wlan's doorbells do the same).
pub fn wmb() {
    unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)) };
}

/// BAR0: 16 MiB of priv registers.
#[derive(Clone, Copy)]
pub struct Regs(pub Mmio);

impl Regs {
    #[inline]
    pub fn rd(&self, r: u32) -> u32 {
        self.0.r32(r as usize)
    }

    #[inline]
    pub fn wr(&self, r: u32, v: u32) {
        self.0.w32(r as usize, v)
    }

    pub fn mask(&self, r: u32, clear: u32, set: u32) -> u32 {
        let old = self.rd(r);
        self.wr(r, old & !clear | set);
        old
    }

    /// `(rd(r) & mask) == val` within `us`.
    pub fn wait(&self, r: u32, mask: u32, val: u32, us: u64, what: &'static str) -> Result<()> {
        if self.0.wait(r as usize, mask, val, us) {
            Ok(())
        } else {
            Err((ETIMEDOUT, what))
        }
    }

    /// `done()` within `us`.
    pub fn until(&self, us: u64, what: &'static str, done: impl FnMut() -> bool) -> Result<()> {
        if time::poll(us, done) {
            Ok(())
        } else {
            Err((ETIMEDOUT, what))
        }
    }

    /// L2 flush of dirty lines to memory (`gf100_ltc_flush`): before the
    /// CPU reads what the GPU wrote.
    pub fn l2_flush(&self) -> Result<()> {
        self.wr(0x70010, 1);
        self.wait(0x70010, 3, 0, 2_000_000, "L2 flush")
    }

    /// L2 invalidate (`gf100_ltc_invalidate`): after the CPU wrote what
    /// the GPU reads through it.
    pub fn l2_invalidate(&self) -> Result<()> {
        wmb();
        self.wr(0x70004, 1);
        self.wait(0x70004, 3, 0, 2_000_000, "L2 invalidate")
    }
}

/// Physically contiguous, uncached memory the GPU reads by physical
/// address or through its MMU. Zeroed on creation.
pub struct Dma {
    shm: SharedMemory,
    pub pa: u64,
}

impl Dma {
    pub fn new(len: usize) -> Result<Self> {
        let len = len.next_multiple_of(4096);
        let shm = SharedMemory::new(len, CONTIGUOUS | UNCACHED).map_err(|e| (-e, "DMA memory"))?;
        let pa = shm.paddr(0).map_err(|e| (-e, "DMA address"))?;
        // The GPU addresses 34 bits.
        if pa + len as u64 > 1 << 34 {
            return Err((EIO, "DMA memory above 16 GiB"));
        }
        Ok(Self { shm, pa })
    }

    pub fn len(&self) -> usize {
        self.shm.len()
    }

    pub fn w32(&self, off: usize, v: u32) {
        assert!(off + 4 <= self.len());
        unsafe { core::ptr::write_volatile(self.shm.as_ptr().add(off) as *mut u32, v) }
    }

    pub fn r32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len());
        unsafe { core::ptr::read_volatile(self.shm.as_ptr().add(off) as *const u32) }
    }

    pub fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }

    pub fn write(&self, off: usize, data: &[u8]) {
        self.shm.as_mut_slice()[off..off + data.len()].copy_from_slice(data);
    }

    pub fn read(&self, off: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.shm.as_slice()[off..off + out.len()]);
    }

    pub fn words(&self, off: usize, w: &[u32]) {
        for (i, v) in w.iter().enumerate() {
            self.w32(off + 4 * i, *v);
        }
    }
}

/// Microseconds since `t0` (ns).
pub fn since_us(t0: u64) -> u64 {
    (time::now_ns() - t0) / 1000
}
