//! Ray-traced sun shadows. Each frame: a pre-pass draws the world
//! positions of what the camera sees at a quarter of the resolution (the
//! solid meshes and the figure), a compute pass shoots one ray toward the
//! sun from each texel (the valley's BVH, and the figure's boxes), and the
//! lit shader filters the answers per pixel (see `nxk_shader::rt::shadow`).

use std::rc::Rc;

use nxgfx::ir::rt::shadow::{self, u, BOXES, BOX_WORDS};
use nxgfx::raytrace::RtScene;
use nxgfx::{
    Attr, Buffer, Cull, Frame, Graphics, Pipeline, PipelineDesc, RenderTarget, Sampling, Shader,
    TexFormat, HEIGHT, WIDTH,
};

use crate::shaders::position_fragment;

/// The mask is this many times smaller than the screen, each way.
pub const SCALE: u32 = 4;
/// The longest shadow ray, in meters: fog hides what is farther.
const REACH: f32 = 90.0;

/// The figure's parts as boxes: (min, max) of each in its own frame, in
/// the order of `player::pose`.
const PARTS: [([f32; 3], [f32; 3]); BOXES] = [
    ([-0.26, 0.0, -0.15], [0.26, 0.66, 0.15]),
    ([-0.16, 0.0, -0.16], [0.16, 0.34, 0.16]),
    ([-0.075, -0.58, -0.075], [0.075, 0.0, 0.075]),
    ([-0.075, -0.58, -0.075], [0.075, 0.0, 0.075]),
    ([-0.105, -0.74, -0.12], [0.105, 0.0, 0.16]),
    ([-0.105, -0.74, -0.12], [0.105, 0.0, 0.16]),
];

pub struct Shadows {
    scene: Rc<RtScene>,
    kernel: Shader,
    pub target: RenderTarget,
    pub mask: Buffer,
    pub pipeline: Pipeline,
    pub w: u32,
    pub h: u32,
}

impl Shadows {
    /// `vs`: the game's vertex shader; `attrs`: its vertex layout.
    pub fn new(
        g: &Graphics,
        scene: Rc<RtScene>,
        vs: &Shader,
        stride: u32,
        attrs: &[(u32, Attr)],
    ) -> nxgfx::Result<Self> {
        // The panel's picture is a quarter turn: the images are 720 wide
        // and 1280 high (see `Graphics::panel`).
        let (w, h) = (HEIGHT / SCALE, WIDTH / SCALE);
        let target = g.render_target(
            w,
            h,
            TexFormat::Rgba32f,
            true,
            Sampling {
                nearest: true,
                clamp: true,
            },
        )?;
        let mask = g.buffer_zeroed((w * h) as usize)?;
        let kernel = g.shader(&shadow::kernel(shadow::Slots::default()))?;
        let fs = g.shader(&position_fragment())?;
        let mut d = PipelineDesc::new(vs, &fs, stride, attrs);
        d.cull = Cull::Back;
        let pipeline = g.pipeline_from(&d)?;
        Ok(Self {
            scene,
            kernel,
            target,
            mask,
            pipeline,
            w,
            h,
        })
    }

    /// The ray pass, after the positions are drawn: `parts`: the figure's
    /// model matrices (rows of a 3 x 4, rotation then translation), `at`
    /// its position.
    pub fn trace(
        &self,
        f: &mut Frame,
        sun: [f32; 3],
        parts: &[[[f32; 4]; 3]; BOXES],
        at: [f32; 3],
    ) {
        let mut un = vec![0.0f32; u::WORDS];
        un[u::SUN as usize..u::SUN as usize + 3].copy_from_slice(&sun);
        un[u::T_MAX as usize] = REACH;
        un[u::INV_W as usize] = 1.0 / self.w as f32;
        un[u::INV_H as usize] = 1.0 / self.h as f32;
        un[u::SPHERE as usize..u::SPHERE as usize + 4].copy_from_slice(&[
            at[0],
            at[1] + 0.95,
            at[2],
            1.35,
        ]);
        for (k, m) in parts.iter().enumerate() {
            let o = u::BOXES as usize + k * BOX_WORDS as usize;
            for a in 0..3 {
                // The world direction of local axis a: column a of the rotation.
                for r in 0..3 {
                    un[o + 3 * a + r] = m[r][a];
                }
                un[o + 12 + a] = PARTS[k].0[a];
                un[o + 15 + a] = PARTS[k].1[a];
            }
            for r in 0..3 {
                un[o + 9 + r] = m[r][3];
            }
        }
        let mut words: Vec<u32> = un.iter().map(|v| v.to_bits()).collect();
        words[u::W as usize] = self.w;
        words[u::H as usize] = self.h;
        f.barrier();
        f.bind_texture(0, self.target.texture());
        f.bind_buffer(0, &self.mask);
        f.uniforms(&words);
        self.scene.bind(f);
        f.dispatch(
            &self.kernel,
            [
                self.w.div_ceil(shadow::GROUP[0]),
                self.h.div_ceil(shadow::GROUP[1]),
                1,
            ],
        );
        f.barrier();
    }
}
