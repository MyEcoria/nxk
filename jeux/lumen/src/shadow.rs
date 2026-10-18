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

/// The angular radius of the sun's disc for the penumbrae, as a tangent
/// (about 1.5 degrees: wider than the real sun, so that it shows).
const SUN_RADIUS: f32 = 0.026;
/// The weight of a new sample in the accumulated mask.
const BLEND: f32 = 0.15;

pub struct Shadows {
    scene: Rc<RtScene>,
    kernel: Shader,
    pub target: RenderTarget,
    /// Two masks, two words a texel (distance, share of the sun): the
    /// kernel writes one and reads the other, then they swap.
    masks: [Buffer; 2],
    cur: usize,
    pub pipeline: Pipeline,
    pub w: u32,
    pub h: u32,
    frame: u32,
    /// The last frame: the view-projection, the camera, the sun and the
    /// figure's sphere (None before the first).
    prev: Option<([[f32; 4]; 4], [f32; 3], [f32; 3], [f32; 4])>,
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
        let masks = [
            g.buffer_zeroed((w * h * 2) as usize)?,
            g.buffer_zeroed((w * h * 2) as usize)?,
        ];
        let kernel = g.shader(&shadow::kernel(shadow::Slots::default()))?;
        let fs = g.shader(&position_fragment())?;
        let mut d = PipelineDesc::new(vs, &fs, stride, attrs);
        d.cull = Cull::Back;
        let pipeline = g.pipeline_from(&d)?;
        Ok(Self {
            scene,
            kernel,
            target,
            masks,
            cur: 0,
            pipeline,
            w,
            h,
            frame: 0,
            prev: None,
        })
    }

    /// The ray pass, after the positions are drawn: `parts`: the figure's
    /// model matrices (rows of a 3 x 4, rotation then translation), `at`
    /// its position; `vp` and `eye`: this frame's camera.
    #[allow(clippy::too_many_arguments)]
    pub fn trace(
        &mut self,
        f: &mut Frame,
        sun: [f32; 3],
        parts: &[[[f32; 4]; 3]; BOXES],
        at: [f32; 3],
        vp: &[[f32; 4]; 4],
        eye: [f32; 3],
        forget: bool,
    ) {
        let sphere = [at[0], at[1] + 0.95, at[2], 1.35];
        let mut un = vec![0u32; u::WORDS];
        let mut set = |at: u16, v: &[f32]| {
            for (k, x) in v.iter().enumerate() {
                un[at as usize + k] = x.to_bits();
            }
        };
        set(u::SUN, &sun);
        set(u::T_MAX, &[REACH]);
        set(u::INV_W, &[1.0 / self.w as f32, 1.0 / self.h as f32]);
        set(u::SPHERE, &sphere);
        for (k, m) in parts.iter().enumerate() {
            let o = u::BOXES + (k as u16) * BOX_WORDS;
            for a in 0..3 {
                // The world direction of local axis a: column a of the rotation.
                let col = [m[0][a], m[1][a], m[2][a]];
                set(o + 3 * a as u16, &col);
                set(o + 12 + a as u16, &[PARTS[k].0[a]]);
                set(o + 15 + a as u16, &[PARTS[k].1[a]]);
            }
            set(o + 9, &[m[0][3], m[1][3], m[2][3]]);
        }
        // The last frame's camera; the history is forgotten on the first
        // frame and when the sun jumped (a change of the time of day).
        let (pvp, pcam, psun, psphere) = self.prev.unwrap_or((*vp, eye, sun, sphere));
        let cos = sun[0] * psun[0] + sun[1] * psun[1] + sun[2] * psun[2];
        let reset = self.prev.is_none() || forget || cos < 0.999_95;
        for (r, row) in pvp.iter().enumerate() {
            set(u::PREV_VP + 4 * r as u16, row);
        }
        set(u::PREV_CAM, &pcam);
        set(u::ALPHA, &[BLEND]);
        set(u::JITTER, &[SUN_RADIUS]);
        set(u::RESET, &[if reset { 1.0 } else { 0.0 }]);
        // Two unit vectors perpendicular to the sun and to each other.
        let a = if sun[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let su = crate::meshes::normalize(crate::meshes::cross(sun, a));
        let sv = crate::meshes::normalize(crate::meshes::cross(sun, su));
        set(u::SUN_U, &su);
        set(u::SUN_V, &sv);
        set(u::PREV_SPHERE, &psphere);
        un[u::W as usize] = self.w;
        un[u::H as usize] = self.h;
        un[u::FRAME as usize] = self.frame;
        self.frame = self.frame.wrapping_add(1);
        self.prev = Some((*vp, eye, sun, sphere));
        let (now, before) = (self.cur, 1 - self.cur);
        f.barrier();
        f.bind_texture(0, self.target.texture());
        f.bind_buffer(0, &self.masks[now]);
        f.bind_buffer(1, &self.masks[before]);
        f.uniforms(&un);
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
        self.cur = before;
    }

    /// What the last mask holds, as text for the log (a test aid): texels
    /// with a distance, and a column of (distance, share) pairs.
    pub fn describe(&self) -> String {
        let m = &self.masks[1 - self.cur];
        let (w, h) = (self.w as usize, self.h as usize);
        let mut all = vec![0u32; w * h * 2];
        m.read_words(0, &mut all);
        let used = (0..w * h)
            .filter(|&i| f32::from_bits(all[2 * i]) > 0.0)
            .count();
        let mut col = String::new();
        let x = w / 2;
        for y in (0..h).step_by(8) {
            let i = y * w + x;
            col.push_str(&format!(
                " {:.1}/{:.2}",
                f32::from_bits(all[2 * i]),
                f32::from_bits(all[2 * i + 1])
            ));
        }
        format!("mask: {} of {} texels have a distance;{}", used, w * h, col)
    }
}
