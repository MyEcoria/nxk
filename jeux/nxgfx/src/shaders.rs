//! Programmable shaders: programs written with `nxk_shader::Builder`
//! (the IR), compiled to the GPU's code by the console's `gpu` service,
//! pipelines made of a vertex and a fragment shader with their draw
//! state, storage buffers, and compute dispatches. A `Frame` binds them
//! (see `Frame::use_pipeline`, `bind_buffer`, `bind_texture`,
//! `uniforms`, `dispatch`).

use alloc::vec::Vec;

use nxk_abi::proto::gpu;
use nxk_abi::shm;
use nxk_abi::user::Msg;
use nxk_shader::{wire, Program, Stage};

use crate::os::{Result, Shm};
use crate::{word, Graphics};

/// A compiled shader. It stays on the GPU until the game closes.
pub struct Shader {
    pub(crate) id: u64,
    pub stage: Stage,
    /// Registers per thread the compiler needed.
    pub registers: u32,
}

/// Memory the GPU reads and writes: vertices, indices, the data of a
/// compute shader. The CPU sees what a finished frame wrote.
pub struct Buffer {
    mem: Shm,
    pub(crate) id: u64,
}

impl Buffer {
    /// Size in bytes.
    pub fn len(&self) -> usize {
        self.mem.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mem.is_empty()
    }

    /// Overwrite words from word `first` (the GPU must be idle: between
    /// frames).
    pub fn write_words(&self, first: usize, w: &[u32]) {
        self.mem.write_words(first * 4, w);
    }

    /// Words from word `first`, as the GPU left them.
    pub fn read_words(&self, first: usize, out: &mut [u32]) {
        assert!((first + out.len()) * 4 <= self.mem.len());
        for (i, o) in out.iter_mut().enumerate() {
            *o = unsafe { core::ptr::read_volatile((self.mem.as_ptr() as *const u32).add(first + i)) };
        }
    }
}

/// How a vertex attribute is stored.
#[derive(Clone, Copy, Debug)]
pub enum Attr {
    F32x1,
    F32x2,
    F32x3,
    F32x4,
    /// Four bytes read as 0..1.
    U8x4Unorm,
}

impl Attr {
    fn code(self) -> u32 {
        match self {
            Attr::F32x1 => gpu::AF_F32X1,
            Attr::F32x2 => gpu::AF_F32X2,
            Attr::F32x3 => gpu::AF_F32X3,
            Attr::F32x4 => gpu::AF_F32X4,
            Attr::U8x4Unorm => gpu::AF_U8X4_UNORM,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cull {
    None,
    Back,
    Front,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    None,
    /// Source alpha over the target.
    Alpha,
    /// Source times its alpha, added.
    Additive,
    /// Source added, target faded by the source alpha.
    Premultiplied,
}

/// A fragment passes the depth test when its depth is ... the stored one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthFunc {
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
    Always,
}

/// What a pipeline is made of.
pub struct PipelineDesc<'a> {
    pub vertex: &'a Shader,
    pub fragment: &'a Shader,
    /// Bytes per vertex.
    pub stride: u32,
    /// Attribute n (byte offset, format) feeds the vertex shader's input n.
    pub attributes: &'a [(u32, Attr)],
    pub cull: Cull,
    pub depth_test: bool,
    pub depth_write: bool,
    pub depth_func: DepthFunc,
    pub blend: Blend,
}

impl<'a> PipelineDesc<'a> {
    pub fn new(
        vertex: &'a Shader,
        fragment: &'a Shader,
        stride: u32,
        attributes: &'a [(u32, Attr)],
    ) -> Self {
        Self {
            vertex,
            fragment,
            stride,
            attributes,
            cull: Cull::Back,
            depth_test: true,
            depth_write: true,
            depth_func: DepthFunc::Less,
            blend: Blend::None,
        }
    }
}

pub struct Pipeline {
    pub(crate) id: u64,
}

impl Graphics {
    /// Compile `program`. The reason is in the console's log when it is
    /// refused (`EINVAL`: it does not validate or compile).
    pub fn shader(&self, program: &Program) -> Result<Shader> {
        let words = wire::encode(program);
        let mem = Shm::new(words.len() * 4, shm::UNCACHED)?;
        mem.write_words(0, &words);
        let m = Msg::from_words(gpu::SHADER_ATTACH, &[words.len() as u64]);
        let r = self.gpu.call_with(m, mem.handle())?;
        Ok(Shader {
            id: word(&r, 0),
            stage: program.stage,
            registers: word(&r, 1) as u32,
        })
    }

    pub fn free_shader(&self, s: Shader) {
        let _ = self.gpu.call(&Msg::from_words(gpu::SHADER_DETACH, &[s.id]));
    }

    /// A graphics pipeline.
    pub fn pipeline_from(&self, d: &PipelineDesc) -> Result<Pipeline> {
        let mut w: Vec<u32> = alloc::vec![0; gpu::PD_ATTRS];
        w[gpu::PD_VERTEX] = d.vertex.id as u32;
        w[gpu::PD_FRAGMENT] = d.fragment.id as u32;
        w[gpu::PD_STRIDE] = d.stride;
        let mut flags = 0;
        match d.cull {
            Cull::None => {}
            Cull::Back => flags |= gpu::PF_CULL_BACK,
            Cull::Front => flags |= gpu::PF_CULL_FRONT,
        }
        if !d.depth_test {
            flags |= gpu::PF_NO_DEPTH_TEST;
        }
        if !d.depth_write {
            flags |= gpu::PF_NO_DEPTH_WRITE;
        }
        w[gpu::PD_FLAGS] = flags;
        w[gpu::PD_BLEND] = match d.blend {
            Blend::None => gpu::BLEND_NONE,
            Blend::Alpha => gpu::BLEND_ALPHA,
            Blend::Additive => gpu::BLEND_ADD,
            Blend::Premultiplied => gpu::BLEND_PREMUL,
        };
        w[gpu::PD_DEPTH] = match d.depth_func {
            DepthFunc::Less => gpu::DEPTH_LESS,
            DepthFunc::LessEqual => gpu::DEPTH_LEQUAL,
            DepthFunc::Greater => gpu::DEPTH_GREATER,
            DepthFunc::GreaterEqual => gpu::DEPTH_GEQUAL,
            DepthFunc::Equal => gpu::DEPTH_EQUAL,
            DepthFunc::Always => gpu::DEPTH_ALWAYS,
        };
        w[gpu::PD_COUNT] = d.attributes.len() as u32;
        for (offset, a) in d.attributes {
            w.push(offset | a.code() << 16);
        }
        let mem = Shm::new(w.len() * 4, shm::UNCACHED)?;
        mem.write_words(0, &w);
        let m = Msg::from_words(gpu::PIPELINE_ATTACH, &[w.len() as u64]);
        let r = self.gpu.call_with(m, mem.handle())?;
        Ok(Pipeline { id: word(&r, 0) })
    }

    /// A buffer holding `data` (copied), for vertices, indices or a
    /// shader's data.
    pub fn buffer_from<T: Copy>(&self, data: &[T]) -> Result<Buffer> {
        let bytes = unsafe {
            core::slice::from_raw_parts(data.as_ptr() as *const u8, core::mem::size_of_val(data))
        };
        let (mem, id) = self.buffer_bytes(bytes)?;
        Ok(Buffer { mem, id })
    }

    /// A zeroed buffer of `words` 32-bit words.
    pub fn buffer_zeroed(&self, words: usize) -> Result<Buffer> {
        let zero = alloc::vec![0u32; words.max(1)];
        self.buffer_from(&zero)
    }
}
