//! The picture on the console's screen, through the GPU: the decoded
//! picture's three planes are written as they are into storage buffers and
//! a fullscreen pass (`nxgfx::ir::stock::yuv420_fragment`) converts,
//! scales and turns them onto the panel. No CPU work on the pixels.

use std::sync::Arc;

use nxgfx::{Buffer, Fullscreen, Graphics, Pipeline, Shader};
use nxh264::Picture;

/// What is built for one picture size.
struct Pass {
    dims: (usize, usize, usize, usize),
    _fragment: Shader,
    pipeline: Pipeline,
    y: Buffer,
    u: Buffer,
    v: Buffer,
}

pub struct Presenter {
    gfx: Graphics,
    fullscreen: Fullscreen,
    pass: Option<Pass>,
}

fn errno(step: &str, e: i64) -> String {
    format!("{} (errno {})", step, -e)
}

impl Presenter {
    /// Powers the GPU and takes the screen.
    pub fn open() -> Result<Self, String> {
        let gfx =
            Graphics::open().map_err(|e| format!("screen: {} (errno {})", e.step, -e.errno))?;
        let fullscreen = gfx.fullscreen().map_err(|e| errno("fullscreen pass", e))?;
        Ok(Self {
            gfx,
            fullscreen,
            pass: None,
        })
    }

    /// The next input event, as `input::READ`'s words.
    pub fn poll(&self, ns: u64) -> Option<[u64; 6]> {
        self.gfx.poll_words(ns)
    }

    /// A plain color (0xRRGGBB) while there is no picture.
    pub fn show_color(&mut self, rgb: u32) -> Result<(), String> {
        let c = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
        let mut f = self.gfx.frame();
        f.clear([c(16), c(8), c(0), 1.0]);
        f.present().map_err(|e| errno("present", e))
    }

    fn build(&mut self, pic: &Picture) -> Result<Pass, String> {
        let (w, h) = (pic.width(), pic.height());
        let (ys, cs) = (pic.y.stride, pic.u.stride);
        let program = nxgfx::ir::stock::yuv420_fragment(w as u32, h as u32, ys as u32, cs as u32);
        let fragment = self
            .gfx
            .shader(&program)
            .map_err(|e| errno("video shader", e))?;
        let pipeline = self
            .gfx
            .pipeline_from(&self.fullscreen.pipeline_desc(&fragment))
            .map_err(|e| errno("video pipeline", e))?;
        let words = |bytes: usize| self.gfx.buffer_zeroed(bytes.div_ceil(4));
        Ok(Pass {
            dims: (w, h, ys, cs),
            y: words(pic.y.data.len()).map_err(|e| errno("luma buffer", e))?,
            u: words(pic.u.data.len()).map_err(|e| errno("chroma buffer", e))?,
            v: words(pic.v.data.len()).map_err(|e| errno("chroma buffer", e))?,
            _fragment: fragment,
            pipeline,
        })
    }

    /// Shows a decoded picture at the next vertical blank.
    pub fn show(&mut self, pic: &Arc<Picture>) -> Result<(), String> {
        let dims = (pic.width(), pic.height(), pic.y.stride, pic.u.stride);
        if self.pass.as_ref().is_none_or(|p| p.dims != dims) {
            self.pass = Some(self.build(pic)?);
        }
        let pass = self.pass.as_ref().expect("pass");
        pass.y.write_bytes(0, &pic.y.data);
        pass.u.write_bytes(0, &pic.u.data);
        pass.v.write_bytes(0, &pic.v.data);
        let mut f = self.gfx.frame();
        f.use_pipeline(&pass.pipeline);
        f.bind_buffer(0, &pass.y);
        f.bind_buffer(1, &pass.u);
        f.bind_buffer(2, &pass.v);
        f.draw_fullscreen(&self.fullscreen);
        f.present().map_err(|e| errno("present", e))
    }
}
