//! Ray queries from compute shaders: the scene's triangles go into a wide
//! BVH built here on the CPU, uploaded once; a `Frame` then traces a
//! buffer of rays against it (`Frame::trace`) and a later pass reads the
//! answers. The GPU has no ray-tracing hardware: this is a compute
//! kernel, a few tens of millions of rays a second at best, so a game
//! shoots a few rays per pixel at reduced resolution (shadows, ambient
//! occlusion) on top of its raster passes.

use nxk_shader::rt::{self, kernel};

pub use nxk_shader::rt::{BuildError, Hit, Ray, Triangle, HIT_WORDS, MISS, RAY_WORDS};

use crate::os::Result;
use crate::shaders::{Buffer, Shader};
use crate::{Frame, Graphics};

/// A scene's triangles in GPU memory.
pub struct RtScene {
    nodes: Buffer,
    tris: Buffer,
    pub triangles: u32,
    pub nodes_count: u32,
    pub depth: u32,
}

/// The compute shaders that answer ray queries.
pub struct RtKernels {
    closest: Shader,
    any: Shader,
}

#[derive(Debug)]
pub enum SceneError {
    Build(BuildError),
    Gpu(i64),
}

impl Graphics {
    /// The tree over `tris`, on the GPU.
    pub fn rt_scene(&self, tris: &[Triangle]) -> core::result::Result<RtScene, SceneError> {
        let bvh = rt::build(tris).map_err(SceneError::Build)?;
        let nodes = self.buffer_from(&bvh.nodes).map_err(SceneError::Gpu)?;
        let tri_buf = self.buffer_from(&bvh.tris).map_err(SceneError::Gpu)?;
        Ok(RtScene {
            nodes,
            tris: tri_buf,
            triangles: bvh.tri_count,
            nodes_count: bvh.node_count,
            depth: bvh.depth,
        })
    }

    /// Compile the query kernels (once per game).
    pub fn rt_kernels(&self) -> Result<RtKernels> {
        let s = kernel::Slots::default();
        Ok(RtKernels {
            closest: self.shader(&kernel::closest_hit(s))?,
            any: self.shader(&kernel::any_hit(s))?,
        })
    }

    /// A buffer for `rays` rays (`RAY_WORDS` words each).
    pub fn rt_rays(&self, rays: usize) -> Result<Buffer> {
        self.buffer_zeroed(rays * RAY_WORDS)
    }

    /// A buffer for the answers of `rays` rays: `HIT_WORDS` words each for
    /// closest-hit queries, one for any-hit ones.
    pub fn rt_hits(&self, rays: usize, any: bool) -> Result<Buffer> {
        self.buffer_zeroed(rays * if any { 1 } else { HIT_WORDS })
    }
}

/// Ray `i` of a ray buffer.
pub fn write_ray(rays: &Buffer, i: usize, r: &Ray) {
    rays.write_words(i * RAY_WORDS, &r.words());
}

impl RtScene {
    /// Bind the tree and the triangles to the slots the ray kernels read
    /// them from (`kernel::Slots`).
    pub fn bind(&self, f: &mut Frame) {
        let s = kernel::Slots::default();
        f.bind_buffer(s.nodes as u32, &self.nodes);
        f.bind_buffer(s.tris as u32, &self.tris);
    }
}

impl Frame<'_> {
    /// Trace the first `count` rays of `rays` against `scene`, answers in
    /// `hits` (see `Graphics::rt_hits`). Uses buffer slots 0-3 and the
    /// uniforms; what the next pass binds replaces them.
    pub fn trace(
        &mut self,
        k: &RtKernels,
        scene: &RtScene,
        rays: &Buffer,
        hits: &Buffer,
        count: u32,
        any: bool,
    ) {
        let s = kernel::Slots::default();
        self.bind_buffer(s.rays as u32, rays);
        self.bind_buffer(s.hits as u32, hits);
        self.bind_buffer(s.nodes as u32, &scene.nodes);
        self.bind_buffer(s.tris as u32, &scene.tris);
        self.uniforms(&[count]);
        let shader = if any { &k.any } else { &k.closest };
        self.dispatch(shader, [count.div_ceil(kernel::GROUP), 1, 1]);
    }
}

/// The answer to ray `i` of a closest-hit query buffer.
pub fn read_hit(hits: &Buffer, i: usize) -> Option<Hit> {
    let mut w = [0u32; HIT_WORDS];
    hits.read_words(i * HIT_WORDS, &mut w);
    (w[3] != MISS).then(|| Hit {
        t: f32::from_bits(w[0]),
        u: f32::from_bits(w[1]),
        v: f32::from_bits(w[2]),
        id: w[3],
    })
}

/// Whether ray `i` of an any-hit query buffer is blocked.
pub fn read_blocked(hits: &Buffer, i: usize) -> bool {
    let mut w = [0u32; 1];
    hits.read_words(i, &mut w);
    w[0] != 0
}
