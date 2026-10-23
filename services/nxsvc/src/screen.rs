//! The screen drawn by the CPU, without the GPU: `display::SCANOUT`'s
//! images, filled in place. For screens that change little (a catalog's
//! own waiting screen); games draw with `nxgfx`.

use nxgfx::os::{received_handle, Service, Shm};
use nxk_abi::proto::display;
use nxk_abi::user::Msg;

use crate::Result;

/// An input event routed to the screen's owner: kind, code, value.
pub type Event = (u64, u64, u64);

pub struct Screen {
    display: Service,
    link: Service,
    images: Vec<Shm>,
    shown: usize,
    released: bool,
}

impl Screen {
    /// Take the screen (`ENODEV`: another program has it).
    pub fn take() -> Result<Self> {
        let display = Service::lookup("display")?;
        let r = display.call(&Msg::new(display::SCANOUT, 0, 0))?;
        let bytes = r.mr[4] as usize;
        let handles: Vec<u32> = (0..display::SCANOUT_BUFFERS).map(received_handle).collect();
        let link = Service::from_handle(received_handle(display::SCANOUT_BUFFERS));
        let mut images = Vec::new();
        for h in handles {
            match Shm::from_handle(h, bytes) {
                Ok(m) => images.push(m),
                Err(e) => {
                    let _ = display.call(&Msg::new(display::RELEASE, 0, 0));
                    return Err(e);
                }
            }
        }
        Ok(Self {
            display,
            link,
            images,
            shown: 0,
            released: false,
        })
    }

    /// The next image filled with one color (0xRRGGBB; a solid color
    /// needs no block-linear swizzle) and shown at the vertical blank.
    pub fn show_color(&mut self, rgb: u32) -> Result<()> {
        let i = (self.shown + 1) % self.images.len();
        let px = 0xFF00_0000 | (rgb & 0xFF) << 16 | (rgb & 0xFF00) | (rgb >> 16) & 0xFF;
        let img = &self.images[i];
        let words = img.len() / 4;
        let p = img.as_ptr() as *mut u32;
        for k in 0..words {
            // Uncached memory: volatile, word by word.
            unsafe { p.add(k).write_volatile(px) };
        }
        self.link
            .call(&Msg::from_words(display::FLIP, &[i as u64]))?;
        self.shown = i;
        Ok(())
    }

    /// The next input event, waiting at most `ns`.
    pub fn event(&self, ns: u64) -> Option<Event> {
        let r = self
            .link
            .call(&Msg::from_words(display::EVENT, &[ns]))
            .ok()?;
        Some((r.mr[0], r.mr[1], r.mr[2]))
    }

    /// The next input event, all its words (kind, code, value, x, y,
    /// source: `input::READ`'s), waiting at most `ns`.
    pub fn event_words(&self, ns: u64) -> Option<[u64; 6]> {
        let r = self
            .link
            .call(&Msg::from_words(display::EVENT, &[ns]))
            .ok()?;
        Some(r.mr)
    }

    /// Give the screen back (also on drop).
    pub fn release(mut self) {
        self.give_back();
    }

    fn give_back(&mut self) {
        if !self.released {
            self.released = true;
            let _ = self.display.call(&Msg::new(display::RELEASE, 0, 0));
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.give_back();
    }
}
