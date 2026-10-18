//! Hybrid raytracing on a GPU with no ray-tracing hardware: a wide BVH
//! built on the CPU, a reference traversal on the CPU, and traversal
//! kernels written in the IR (shadow and closest-hit queries for the
//! compute shaders a game dispatches after its raster passes).
//!
//! Buffer layouts (32-bit words):
//! * nodes: 32 words per node, four children in structure-of-arrays
//!   form: references 0-3, then min x, min y, min z, max x, max y, max z
//!   of the four boxes (4 words each), 4 unused. A reference is
//!   `EMPTY`, an inner node's index, or a leaf: `LEAF | first << 3 |
//!   (count - 1)`, a range of `tris`. The root is node 0.
//! * triangles: 12 words each: v0 (3), v1 - v0 (3), v2 - v0 (3), the id
//!   the game gave it, 2 unused.
//! * rays: 8 words: origin (3), t min, direction (3), t max.
//! * hits: 4 words: t, u, v (barycentric), id (`MISS` for none).

pub mod bvh;
pub mod demo;
pub mod kernel;
pub mod trace;

pub use bvh::{build, BuildError, Bvh, Triangle};

pub const NODE_WORDS: usize = 32;
pub const TRI_WORDS: usize = 12;
pub const RAY_WORDS: usize = 8;
pub const HIT_WORDS: usize = 4;
pub const EMPTY: u32 = 0xFFFF_FFFF;
pub const LEAF: u32 = 0x8000_0000;
pub const MISS: u32 = 0xFFFF_FFFF;
/// Triangles one leaf holds, at most.
pub const LEAF_MAX: u32 = 8;
/// Levels of the tree, at most: the traversal stack holds three entries
/// per level.
pub const MAX_DEPTH: u32 = 18;
/// Words of local memory a traversal kernel uses: the stack, with room
/// for the entries a step writes before it knows how many count.
pub const STACK_WORDS: u32 = 3 * MAX_DEPTH + 4;

/// A ray: where it starts, which way (not necessarily unit), the range of
/// distances along it that count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    pub origin: [f32; 3],
    pub t_min: f32,
    pub dir: [f32; 3],
    pub t_max: f32,
}

impl Ray {
    pub fn words(&self) -> [u32; RAY_WORDS] {
        [
            self.origin[0].to_bits(),
            self.origin[1].to_bits(),
            self.origin[2].to_bits(),
            self.t_min.to_bits(),
            self.dir[0].to_bits(),
            self.dir[1].to_bits(),
            self.dir[2].to_bits(),
            self.t_max.to_bits(),
        ]
    }
}

/// The nearest hit of a ray.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub t: f32,
    pub u: f32,
    pub v: f32,
    pub id: u32,
}

#[cfg(test)]
mod tests;
