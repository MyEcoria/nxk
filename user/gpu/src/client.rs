//! What games share with the engine: buffers (vertices, indices, command
//! streams), render targets, and the command stream that draws with
//! them (`abi::proto::gpu::CMD_*`). Everything a stream names is checked
//! before anything runs.

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ENOSPC};
use libnx::abi::prot;
use libnx::abi::proto::gpu as proto;
use libnx::mem::SharedMemory;
use libnx::sys;

use nxk_shader::lower::{self, Compiled};
use nxk_shader::{wire, Stage};

use crate::engine::Compute;
use crate::gfx::{
    attr_word, color_format, Color, Custom, Depth, Gfx, Instances, Layout, Pipe, Target, TexImage,
    Tiled, ATTR_INACTIVE, INSTANCE_STREAM, LIT_UBO_WORDS, MAX_TEXTURES,
};
use crate::mmu::{self, Vm, PAGE};
use crate::push::Push;
use crate::qmd::{Launch3, Qmd};

const MAX_BUFFERS: usize = 64;
const MAX_TARGETS: usize = 16;
/// Pushbuffer words a stream may fill (the pushbuffer holds 64 Ki).
const MAX_WORDS: usize = 48_000;
/// What a stream may fill at most, instanced draws included.
const MAX_PUSH_WORDS: usize = 64_000;
/// Largest uniform block of a draw, in words.
const MAX_UNIFORM_WORDS: usize = 1024;
const MAX_SHADERS: usize = 64;
const MAX_PIPELINES: usize = 64;
/// Words of one shader's wire form (4096 instructions of up to 8 words).
const MAX_SHADER_WORDS: u64 = 40_000;
/// Registers a compute group may use together (one SM's file).
const GROUP_REGISTERS: u32 = 65_536;

type Res<T> = core::result::Result<T, i64>;

/// A shm handle held until the entry goes, mapped for reading if it came
/// with the right.
enum Held {
    Mapped(SharedMemory),
    Raw(u32),
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Held::Raw(h) = self {
            let _ = sys::handle_close(*h);
        }
    }
}

struct Buf {
    va: u64,
    len: u64,
    held: Held,
    /// Physical pages, and whether they are mapped cached in L2.
    pg: Vec<u64>,
    cached: bool,
}

#[derive(Clone, Copy)]
enum Image {
    Color(Tiled),
    Depth(Depth),
}

struct Tgt {
    image: Image,
    bytes: u64,
    _held: Held,
    pg: Vec<u64>,
}

struct Tex {
    va: u64,
    bytes: u64,
    _held: Held,
    pg: Vec<u64>,
    cached: bool,
}

/// A compiled shader of the client, in a program region.
struct Shader {
    stage: Stage,
    /// Graphics: the header's offset; compute: the program's.
    at: u32,
    /// What to give back to the region.
    block: u32,
    len: u32,
    gprs: u32,
    local_bytes: u32,
    block_dim: [u32; 3],
    uniform_words: u32,
    /// Pipelines built on it.
    users: u32,
}

struct Pipeline {
    custom: Custom,
    vs: u32,
    fs: u32,
    uniform_words: u32,
}

#[derive(Default)]
pub struct Client {
    bufs: Vec<Option<Buf>>,
    tgts: Vec<Option<Tgt>>,
    texs: Vec<Option<Tex>>,
    shaders: Vec<Option<Shader>>,
    pipelines: Vec<Option<Pipeline>>,
}

/// The pipeline draws go through.
#[derive(Clone, Copy)]
enum Cur {
    Builtin(Pipe),
    Custom(u32),
}

/// Physical pages of `len` bytes of a client's shm (DMA right).
fn pages(h: u32, len: u64) -> Res<Vec<u64>> {
    if len == 0 {
        return Err(-EINVAL);
    }
    sys::shm_paddr(h, len - 1)?;
    (0..len.div_ceil(PAGE))
        .map(|k| sys::shm_paddr(h, k * PAGE))
        .collect()
}

fn slot<T>(v: &mut Vec<Option<T>>, max: usize) -> Res<usize> {
    match v.iter().position(|e| e.is_none()) {
        Some(i) => Ok(i),
        None if v.len() < max => {
            v.push(None);
            Ok(v.len() - 1)
        }
        None => Err(-ENOSPC),
    }
}

impl Client {
    /// `h`: the client's shm, which this takes (closed on error too).
    pub fn attach_buffer(&mut self, vm: &mut Vm, h: u32, len: u64) -> Res<u64> {
        let held = SharedMemory::map(h, len as usize, prot::READ)
            .map(Held::Mapped)
            .unwrap_or_else(|_| Held::Raw(h));
        let pg = pages(h, len)?;
        let i = slot(&mut self.bufs, MAX_BUFFERS)?;
        let va = vm
            .alloc(pg.len() as u64 * PAGE, PAGE)
            .map_err(|(e, _)| -e)?;
        // Data the GPU reads (and compute writes) is cached in L2; memory
        // that is also a render target stays volatile.
        let cached = !self.shares_target(&pg);
        if cached {
            vm.map_at_cached(va, &pg, 0)
        } else {
            vm.map_at(va, &pg, false)
        }
        .map_err(|(e, _)| -e)?;
        self.bufs[i] = Some(Buf {
            va,
            len,
            held,
            pg,
            cached,
        });
        Ok(i as u64)
    }

    /// Whether any of `pg` backs a render target.
    fn shares_target(&self, pg: &[u64]) -> bool {
        let mut t: Vec<u64> = self
            .tgts
            .iter()
            .flatten()
            .flat_map(|t| t.pg.iter().copied())
            .collect();
        t.sort_unstable();
        pg.iter().any(|p| t.binary_search(p).is_ok())
    }

    /// A new target's pages: buffers and textures on the same memory
    /// go back to volatile mappings (a target is written past L2).
    fn make_volatile(&mut self, vm: &mut Vm, pg: &[u64]) {
        let mut sorted = pg.to_vec();
        sorted.sort_unstable();
        let hit = |p: &[u64]| p.iter().any(|x| sorted.binary_search(x).is_ok());
        for b in self.bufs.iter_mut().flatten() {
            if b.cached && hit(&b.pg) {
                let _ = vm.map_at(b.va, &b.pg, false);
                b.cached = false;
            }
        }
        for t in self.texs.iter_mut().flatten() {
            if t.cached && hit(&t.pg) {
                let _ = vm.map_at_kind(t.va, &t.pg, false, mmu::KIND_GENERIC_16BX2);
                t.cached = false;
            }
        }
    }

    pub fn detach_buffer(&mut self, vm: &mut Vm, id: u64) -> Res<()> {
        let b = self
            .bufs
            .get_mut(id as usize)
            .and_then(|b| b.take())
            .ok_or(-EINVAL)?;
        let _ = vm.unmap(b.va, b.len);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn attach_target(
        &mut self,
        vm: &mut Vm,
        h: u32,
        width: u32,
        height: u32,
        kind: u64,
        block_log2: u32,
        format: u64,
    ) -> Res<u64> {
        let held = Held::Raw(h);
        if width == 0 || height == 0 || width > 16384 || height > 16384 || block_log2 > 5 {
            return Err(-EINVAL);
        }
        let (mk, bl, image) = match kind {
            proto::TARGET_COLOR => {
                let code = color_format(format).ok_or(-EINVAL)?;
                // A block taller than the image needs is not what the
                // sampler reads it as.
                if proto::level_block_log2(block_log2, height) != block_log2 {
                    return Err(-EINVAL);
                }
                (
                    mmu::KIND_GENERIC_16BX2,
                    block_log2,
                    Image::Color(Tiled {
                        va: 0,
                        width,
                        height,
                        format: code,
                        bpp: proto::tf_bytes(format),
                        block_log2,
                    }),
                )
            }
            proto::TARGET_DEPTH if format == proto::TF_RGBA8 => (
                mmu::KIND_ZF32,
                proto::DEPTH_BLOCK_LOG2,
                Image::Depth(Depth {
                    va: 0,
                    width,
                    height,
                }),
            ),
            _ => return Err(-EINVAL),
        };
        let bytes = match &image {
            Image::Color(c) => c.bytes(),
            Image::Depth(_) => proto::tiled_bytes(width, height, bl),
        } as u64;
        let pg = pages(h, bytes)?;
        let i = slot(&mut self.tgts, MAX_TARGETS)?;
        let va = vm.alloc(bytes, 1 << 17).map_err(|(e, _)| -e)?;
        vm.map_at_kind(va, &pg, false, mk).map_err(|(e, _)| -e)?;
        self.make_volatile(vm, &pg);
        let image = match image {
            Image::Color(c) => Image::Color(Tiled { va, ..c }),
            Image::Depth(d) => Image::Depth(Depth { va, ..d }),
        };
        self.tgts[i] = Some(Tgt {
            image,
            bytes,
            _held: held,
            pg,
        });
        Ok(i as u64)
    }

    pub fn detach_target(&mut self, vm: &mut Vm, id: u64) -> Res<()> {
        let t = self
            .tgts
            .get_mut(id as usize)
            .and_then(|t| t.take())
            .ok_or(-EINVAL)?;
        let va = match t.image {
            Image::Color(c) => c.va,
            Image::Depth(d) => d.va,
        };
        let _ = vm.unmap(va, t.bytes);
        Ok(())
    }

    /// `h`: the client's shm with the image, which this takes (closed on
    /// error too). The pool entries go in `gfx`. `block`: 0 for the
    /// automatic block height, else log2 of GOBs + 1; `levels`: 0 for one.
    #[allow(clippy::too_many_arguments)]
    pub fn attach_texture(
        &mut self,
        vm: &mut Vm,
        gfx: &Gfx,
        h: u32,
        width: u32,
        height: u32,
        flags: u64,
        format: u64,
        block: u64,
        levels: u64,
    ) -> Res<u64> {
        let held = Held::Raw(h);
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || flags & !(proto::TEX_NEAREST | proto::TEX_CLAMP) != 0
            || block > 6
            || levels > proto::max_levels(width, height) as u64
        {
            return Err(-EINVAL);
        }
        let levels = levels.max(1) as u32;
        let bpp = proto::tf_bytes(format);
        color_format(format).ok_or(-EINVAL)?;
        let block_log2 = if block == 0 {
            proto::tex_block_log2(height)
        } else {
            block as u32 - 1
        };
        if proto::level_block_log2(block_log2, height) != block_log2 {
            return Err(-EINVAL);
        }
        let bytes = proto::mip_bytes(width, height, bpp, block_log2, levels) as u64;
        let pg = pages(h, bytes)?;
        let i = slot(&mut self.texs, MAX_TEXTURES)?;
        let va = vm.alloc(bytes, 1 << 17).map_err(|(e, _)| -e)?;
        let cached = !self.shares_target(&pg);
        if cached {
            vm.map_at_cached(va, &pg, mmu::KIND_GENERIC_16BX2)
        } else {
            vm.map_at_kind(va, &pg, false, mmu::KIND_GENERIC_16BX2)
        }
        .map_err(|(e, _)| -e)?;
        let img = TexImage {
            va,
            width,
            height,
            format,
            block_log2,
            levels,
        };
        gfx.set_texture(i, &img, flags);
        self.texs[i] = Some(Tex {
            va,
            bytes,
            _held: held,
            pg,
            cached,
        });
        Ok(i as u64)
    }

    pub fn detach_texture(&mut self, vm: &mut Vm, gfx: &Gfx, id: u64) -> Res<()> {
        let t = self
            .texs
            .get_mut(id as usize)
            .and_then(|t| t.take())
            .ok_or(-EINVAL)?;
        gfx.clear_texture(id as usize);
        let _ = vm.unmap(t.va, t.bytes);
        Ok(())
    }

    /// Compile the shader whose wire words are in the client's shm `h`
    /// (taken, closed on error too).
    pub fn attach_shader(
        &mut self,
        gfx: &mut Gfx,
        compute: &mut Compute,
        h: u32,
        words: u64,
    ) -> Res<(u64, u32, u32)> {
        if words == 0 || words > MAX_SHADER_WORDS {
            let _ = sys::handle_close(h);
            return Err(-EINVAL);
        }
        let m = match SharedMemory::map(h, words as usize * 4, prot::READ) {
            Ok(m) => m,
            Err(e) => {
                let _ = sys::handle_close(h);
                return Err(e);
            }
        };
        // A copy: the client could change it while it is checked.
        let w: Vec<u32> =
            unsafe { core::slice::from_raw_parts(m.as_ptr() as *const u32, words as usize) }
                .to_vec();
        drop(m);
        let prog = wire::decode(&w).map_err(|e| {
            libnx::drv::report("gpu", &alloc::format!("shader: {e}"));
            -EINVAL
        })?;
        let c: Compiled = lower::compile(&prog).map_err(|e| {
            libnx::drv::report("gpu", &alloc::format!("shader: {e:?}"));
            -EINVAL
        })?;
        let gprs = c.gprs.next_multiple_of(8).max(8);
        let block_dim = prog.local_size.map(|v| v as u32);
        let (at, block, len) = match c.stage {
            Stage::Compute => {
                if gprs * block_dim.iter().product::<u32>() > GROUP_REGISTERS {
                    return Err(-EINVAL);
                }
                let (at, len) = compute.install(&c.code).ok_or(-ENOSPC)?;
                (at, at, len)
            }
            _ => gfx.install(&c.header, &c.code).ok_or(-ENOSPC)?,
        };
        let i = match slot(&mut self.shaders, MAX_SHADERS) {
            Ok(i) => i,
            Err(e) => {
                match c.stage {
                    Stage::Compute => compute.uninstall(block, len),
                    _ => gfx.uninstall(block, len),
                }
                return Err(e);
            }
        };
        self.shaders[i] = Some(Shader {
            stage: c.stage,
            at,
            block,
            len,
            gprs,
            local_bytes: c.local_bytes,
            block_dim,
            uniform_words: c.info.uniform_words as u32,
            users: 0,
        });
        Ok((i as u64, gprs, c.stage as u32))
    }

    pub fn detach_shader(&mut self, gfx: &mut Gfx, compute: &mut Compute, id: u64) -> Res<()> {
        let sh = self
            .shaders
            .get_mut(id as usize)
            .ok_or(-EINVAL)?
            .as_ref()
            .ok_or(-EINVAL)?;
        if sh.users > 0 {
            return Err(-libnx::abi::err::EBUSY);
        }
        let sh = self.shaders[id as usize].take().unwrap();
        match sh.stage {
            Stage::Compute => compute.uninstall(sh.block, sh.len),
            _ => gfx.uninstall(sh.block, sh.len),
        }
        Ok(())
    }

    /// A pipeline from the description in the client's shm `h` (taken).
    pub fn attach_pipeline(&mut self, h: u32, words: u64) -> Res<u64> {
        if words < proto::PD_ATTRS as u64 || words > 64 {
            let _ = sys::handle_close(h);
            return Err(-EINVAL);
        }
        let m = match SharedMemory::map(h, words as usize * 4, prot::READ) {
            Ok(m) => m,
            Err(e) => {
                let _ = sys::handle_close(h);
                return Err(e);
            }
        };
        let w: Vec<u32> =
            unsafe { core::slice::from_raw_parts(m.as_ptr() as *const u32, words as usize) }
                .to_vec();
        drop(m);
        let n = w[proto::PD_COUNT] as usize;
        if n > proto::MAX_ATTRIBUTES || w.len() < proto::PD_ATTRS + n {
            return Err(-EINVAL);
        }
        let stride = w[proto::PD_STRIDE];
        if stride == 0 || stride > 1024 || !stride.is_multiple_of(4) {
            return Err(-EINVAL);
        }
        let inst_stride = w[proto::PD_INSTANCE_STRIDE];
        if inst_stride > 1024 || !inst_stride.is_multiple_of(4) {
            return Err(-EINVAL);
        }
        let mut attrs = Vec::new();
        let mut per_instance = false;
        for k in 0..n {
            let a = w[proto::PD_ATTRS + k];
            let inst = a & proto::AT_INSTANCE != 0;
            let (offset, format) = (a & 0xFFFF, (a >> 16) & 0x7FFF);
            let mut word = attr_word(offset, format).ok_or(-EINVAL)?;
            // The attribute lies inside the vertex or the instance.
            let size = match format {
                proto::AF_F32X1 | proto::AF_U8X4_UNORM => 4,
                proto::AF_F32X2 => 8,
                proto::AF_F32X3 => 12,
                _ => 16,
            };
            if inst {
                per_instance = true;
                word |= INSTANCE_STREAM;
            }
            if offset + size > if inst { inst_stride } else { stride } {
                return Err(-EINVAL);
            }
            attrs.push(word);
        }
        // A stride without an attribute that uses it is a mistake.
        if per_instance != (inst_stride != 0) {
            return Err(-EINVAL);
        }
        attrs.resize(proto::MAX_ATTRIBUTES, ATTR_INACTIVE);
        let (vs, fs) = (w[proto::PD_VERTEX], w[proto::PD_FRAGMENT]);
        let get = |id: u32, stage: Stage| -> Res<&Shader> {
            match self.shaders.get(id as usize).and_then(|s| s.as_ref()) {
                Some(s) if s.stage == stage => Ok(s),
                _ => Err(-EINVAL),
            }
        };
        let (v, f) = (get(vs, Stage::Vertex)?, get(fs, Stage::Fragment)?);
        let blend = w[proto::PD_BLEND];
        if blend > proto::BLEND_PREMUL || w[proto::PD_DEPTH] > proto::DEPTH_ALWAYS {
            return Err(-EINVAL);
        }
        let custom = Custom {
            vs: v.at,
            vs_gprs: v.gprs,
            fs: f.at,
            fs_gprs: f.gprs,
            stride,
            attrs,
            inst_stride,
            flags: w[proto::PD_FLAGS],
            blend,
            depth: w[proto::PD_DEPTH],
        };
        let uniform_words = v.uniform_words.max(f.uniform_words);
        let i = slot(&mut self.pipelines, MAX_PIPELINES)?;
        for id in [vs, fs] {
            if let Some(Some(s)) = self.shaders.get_mut(id as usize) {
                s.users += 1;
            }
        }
        self.pipelines[i] = Some(Pipeline {
            custom,
            vs,
            fs,
            uniform_words,
        });
        Ok(i as u64)
    }

    pub fn detach_pipeline(&mut self, id: u64) -> Res<()> {
        let p = self
            .pipelines
            .get_mut(id as usize)
            .and_then(|p| p.take())
            .ok_or(-EINVAL)?;
        for sid in [p.vs, p.fs] {
            if let Some(Some(s)) = self.shaders.get_mut(sid as usize) {
                s.users = s.users.saturating_sub(1);
            }
        }
        Ok(())
    }

    /// Everything went: handles and GPU mappings.
    pub fn clear(&mut self, vm: &mut Vm, gfx: &mut Gfx, compute: &mut Compute) {
        for id in 0..self.pipelines.len() as u64 {
            let _ = self.detach_pipeline(id);
        }
        for id in 0..self.shaders.len() as u64 {
            let _ = self.detach_shader(gfx, compute, id);
        }
        for id in 0..self.texs.len() as u64 {
            let _ = self.detach_texture(vm, gfx, id);
        }
        for id in 0..self.bufs.len() as u64 {
            let _ = self.detach_buffer(vm, id);
        }
        for id in 0..self.tgts.len() as u64 {
            let _ = self.detach_target(vm, id);
        }
    }

    fn buf(&self, id: u32) -> Res<&Buf> {
        self.bufs
            .get(id as usize)
            .and_then(|b| b.as_ref())
            .ok_or(-EINVAL)
    }

    /// `len` bytes at `off` of buffer `id`, as a GPU address.
    fn span(&self, id: u32, off: u32, len: u64) -> Res<u64> {
        let b = self.buf(id)?;
        if !off.is_multiple_of(4) || off as u64 + len > b.len {
            return Err(-EINVAL);
        }
        Ok(b.va + off as u64)
    }

    /// The command words `first..first + count` of buffer `id`.
    fn words(&self, id: u32, first: u32, count: u32) -> Res<&[u32]> {
        let b = self.buf(id)?;
        let Held::Mapped(m) = &b.held else {
            return Err(-EINVAL);
        };
        let end = (first as u64 + count as u64) * 4;
        if end > b.len {
            return Err(-EINVAL);
        }
        let all =
            unsafe { core::slice::from_raw_parts(m.as_ptr() as *const u32, (b.len / 4) as usize) };
        Ok(&all[first as usize..(first + count) as usize])
    }

    fn target(&self, color: u32, depth: u32) -> Res<Target> {
        let get = |id: u32| {
            self.tgts
                .get(id as usize)
                .and_then(|t| t.as_ref())
                .map(|t| t.image)
                .ok_or(-EINVAL)
        };
        let Image::Color(c) = get(color)? else {
            return Err(-EINVAL);
        };
        let depth = if depth == proto::NO_TARGET {
            None
        } else {
            match get(depth)? {
                Image::Depth(d) if d.width == c.width && d.height == c.height => Some(d),
                _ => return Err(-EINVAL),
            }
        };
        Ok(Target {
            color: Color::Tiled(c),
            depth,
        })
    }

    /// The shaders' constant buffer, if the stream changed it since the
    /// last draw: its words, and for `Textured` the handle of the bound
    /// texture after the `LIT_UBO_WORDS`.
    fn flush(
        gfx: &mut Gfx,
        p: &mut Push,
        pipe: Pipe,
        uniforms: &[u32],
        texture: Option<u32>,
        dirty: &mut bool,
    ) -> Res<()> {
        if !core::mem::take(dirty) || uniforms.is_empty() {
            return Ok(());
        }
        let ok = if pipe == Pipe::Textured {
            let id = texture.ok_or(-EINVAL)?;
            let mut w = uniforms.to_vec();
            w.resize(LIT_UBO_WORDS, 0);
            w.push(Gfx::texture_handle(id as usize));
            gfx.uniforms(p, &w)
        } else {
            gfx.uniforms(p, uniforms)
        };
        if ok {
            Ok(())
        } else {
            Err(-ENOSPC)
        }
    }

    /// Constant buffers 2 and 3 of the shaders: a descriptor (address
    /// low, high, length in words) per bound buffer, a handle per bound
    /// texture. Unbound slots are empty: a shader reads zeros there.
    fn tables(&self, b: &Binds) -> Res<(Vec<u32>, Vec<u32>)> {
        let mut desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
        for (k, id) in b.bufs.iter().enumerate() {
            if let Some(id) = id {
                let buf = self.buf(*id)?;
                desc[4 * k] = buf.va as u32;
                desc[4 * k + 1] = (buf.va >> 32) as u32;
                desc[4 * k + 2] = (buf.len / 4) as u32;
            }
        }
        let mut handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
        for (k, id) in b.texs.iter().enumerate() {
            if let Some(id) = id {
                handles[k] = Gfx::texture_handle(*id as usize);
            }
        }
        Ok((desc, handles))
    }

    /// Uniforms and bindings for the next custom draw, if changed.
    fn flush_custom(&self, gfx: &mut Gfx, p: &mut Push, pl: &Pipeline, st: &mut Draw) -> Res<()> {
        if core::mem::take(&mut st.dirty) {
            let mut w = st.uniforms.clone();
            w.resize(w.len().max(pl.uniform_words as usize).max(1), 0);
            if !gfx.uniforms(p, &w) {
                return Err(-ENOSPC);
            }
        }
        if core::mem::take(&mut st.binds.dirty) {
            let (desc, handles) = self.tables(&st.binds)?;
            if !gfx.cbuf(p, 2, &desc) || !gfx.cbuf(p, 3, &handles) {
                return Err(-ENOSPC);
            }
        }
        Ok(())
    }

    /// The instance data of an instanced draw of `n` instances with
    /// pipeline `pl`: `buf` at `off` holds `n` strides of it, or is
    /// `NO_TARGET` when the pipeline reads none. `used`: words pushed.
    fn instances(&self, pl: &Pipeline, used: usize, buf: u32, off: u32, n: u32) -> Res<Instances> {
        if n == 0 || n > proto::MAX_INSTANCES || used + Gfx::instanced_words(n) > MAX_PUSH_WORDS {
            return Err(-EINVAL);
        }
        let stride = pl.custom.inst_stride;
        let va = match (stride, buf) {
            (0, proto::NO_TARGET) => 0,
            (0, _) | (_, proto::NO_TARGET) => return Err(-EINVAL),
            _ => self.span(buf, off, n as u64 * stride as u64)?,
        };
        Ok(Instances { va, count: n })
    }

    fn pipeline(&self, id: u32) -> Res<&Pipeline> {
        self.pipelines
            .get(id as usize)
            .and_then(|p| p.as_ref())
            .ok_or(-EINVAL)
    }

    /// Launch compute shader `sid` over `groups` with the uniforms and
    /// bindings of the stream so far.
    fn dispatch(
        &self,
        gfx: &mut Gfx,
        compute: &Compute,
        p: &mut Push,
        sid: u32,
        groups: [u32; 3],
        st: &Draw,
    ) -> Res<()> {
        let sh = match self.shaders.get(sid as usize).and_then(|s| s.as_ref()) {
            Some(s) if s.stage == Stage::Compute => s,
            _ => return Err(-EINVAL),
        };
        if groups.iter().any(|&g| g == 0 || g > 65_535) {
            return Err(-EINVAL);
        }
        let (desc, handles) = self.tables(&st.binds)?;
        let mut u = st.uniforms.clone();
        u.resize(u.len().max(sh.uniform_words as usize).max(1), 0);
        let stage = |gfx: &mut Gfx, w: &[u32]| gfx.stage(w).ok_or(-ENOSPC);
        // Constant buffer 0 is unused; 1: uniforms; 2 and 3: bindings.
        let cbs = [
            stage(gfx, &[0; 64])?,
            stage(gfx, &u)?,
            stage(gfx, &desc)?,
            stage(gfx, &handles)?,
        ];
        let (at, qva) = gfx.alloc_ubo(crate::qmd::SIZE).ok_or(-ENOSPC)?;
        let q = Qmd::new3(&Launch3 {
            program: sh.at,
            regs: sh.gprs,
            grid: groups,
            block: sh.block_dim,
            cbs: &cbs,
            local_bytes: sh.local_bytes,
        });
        gfx.ubo_write(at, &q.bytes());
        // Earlier draws are done first, later ones see the results.
        gfx.barrier(p);
        compute.launch(p, qva);
        gfx.barrier(p);
        Ok(())
    }

    /// The batch for the stream `first..first + count` of buffer `id`,
    /// or why it is refused; nothing has run.
    pub fn build(
        &self,
        gfx: &mut Gfx,
        compute: &Compute,
        id: u32,
        first: u32,
        count: u32,
    ) -> Res<Push> {
        let w = self.words(id, first, count)?;
        let mut p = Push::default();
        gfx.begin(&mut p);
        let mut at = 0;
        let mut target: Option<Target> = None;
        let mut bound: Option<(u32, u32)> = None;
        let mut cur: Option<Cur> = None;
        // Uniforms are written before a draw, once they or the texture
        // changed.
        let mut texture: Option<u32> = None;
        let mut st = Draw::default();
        while at < w.len() {
            let (op, n) = (w[at] & 0xFFFF, (w[at] >> 16) as usize);
            let a = w.get(at + 1..at + 1 + n).ok_or(-EINVAL)?;
            at += 1 + n;
            if p.words.len() > MAX_WORDS {
                return Err(-ENOSPC);
            }
            match (op, a) {
                (proto::CMD_TARGET, &[c, z]) => {
                    target = Some(self.target(c, z)?);
                    // The pass before may have drawn what this one samples.
                    if bound.is_some_and(|b| b != (c, z)) {
                        gfx.barrier(&mut p);
                    }
                    bound = Some((c, z));
                    // Draws wait for a pipeline set on this target.
                    cur = None;
                }
                (proto::CMD_CLEAR, &[r, g, b, al]) => {
                    let t = target.as_ref().ok_or(-EINVAL)?;
                    gfx.clear(&mut p, t, [r, g, b, al].map(f32::from_bits));
                }
                (proto::CMD_PIPELINE, &[kind, flags]) => {
                    let t = target.as_ref().ok_or(-EINVAL)?;
                    let k = match kind {
                        proto::PIPE_FLAT => Pipe::Flat,
                        proto::PIPE_LIT => Pipe::Lit,
                        proto::PIPE_TEXTURED => Pipe::Textured,
                        _ => return Err(-EINVAL),
                    };
                    st.dirty |= !matches!(cur, Some(Cur::Builtin(c)) if c == k);
                    cur = Some(Cur::Builtin(k));
                    gfx.pipeline(&mut p, t, k, flags & proto::PF_CULL_BACK != 0);
                }
                (proto::CMD_USE_PIPELINE, &[pid]) => {
                    let t = target.as_ref().ok_or(-EINVAL)?;
                    let pl = self.pipeline(pid)?;
                    gfx.pipeline_custom(&mut p, t, &pl.custom);
                    st.dirty = true;
                    st.binds.dirty = true;
                    cur = Some(Cur::Custom(pid));
                }
                (proto::CMD_UNIFORMS, u) if !u.is_empty() && u.len() <= MAX_UNIFORM_WORDS => {
                    st.uniforms.clear();
                    st.uniforms.extend_from_slice(u);
                    st.dirty = true;
                }
                (proto::CMD_TEXTURE, &[id]) => {
                    let live = self.texs.get(id as usize).is_some_and(|t| t.is_some());
                    if !live {
                        return Err(-EINVAL);
                    }
                    texture = Some(id);
                    st.dirty = true;
                }
                (proto::CMD_BIND_BUFFER, &[slot, id]) => {
                    let slot = slot as usize;
                    if slot >= proto::SHADER_BUFFERS {
                        return Err(-EINVAL);
                    }
                    st.binds.bufs[slot] = if id == proto::NO_TARGET {
                        None
                    } else {
                        self.buf(id)?;
                        Some(id)
                    };
                    st.binds.dirty = true;
                }
                (proto::CMD_BIND_TEXTURE, &[slot, id]) => {
                    let slot = slot as usize;
                    if slot >= proto::SHADER_TEXTURES {
                        return Err(-EINVAL);
                    }
                    st.binds.texs[slot] = if id == proto::NO_TARGET {
                        None
                    } else {
                        let live = self.texs.get(id as usize).is_some_and(|t| t.is_some());
                        if !live {
                            return Err(-EINVAL);
                        }
                        Some(id)
                    };
                    st.binds.dirty = true;
                }
                (proto::CMD_DISPATCH, &[sid, x, y, z]) => {
                    self.dispatch(gfx, compute, &mut p, sid, [x, y, z], &st)?;
                }
                (proto::CMD_BARRIER, &[]) => gfx.barrier(&mut p),
                (proto::CMD_DRAW, &[vb, off, vertices, f, c]) => {
                    let cu = cur.ok_or(-EINVAL)?;
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > vertices as u64 {
                        return Err(-EINVAL);
                    }
                    match cu {
                        Cur::Builtin(k) => {
                            Self::flush(gfx, &mut p, k, &st.uniforms, texture, &mut st.dirty)?;
                            let va = self.span(vb, off, vertices as u64 * k.stride() as u64)?;
                            gfx.draw(&mut p, k, va, vertices, f, c);
                        }
                        Cur::Custom(pid) => {
                            let pl = self.pipeline(pid)?;
                            if pl.custom.inst_stride != 0 {
                                return Err(-EINVAL);
                            }
                            self.flush_custom(gfx, &mut p, pl, &mut st)?;
                            let lay = Layout {
                                stride: pl.custom.stride,
                                attrs: &pl.custom.attrs,
                                inst_stride: 0,
                            };
                            let va = self.span(vb, off, vertices as u64 * lay.stride as u64)?;
                            gfx.draw_layout(&mut p, &lay, va, vertices, f, c);
                        }
                    }
                }
                (proto::CMD_DRAW_INDEXED, &[vb, off, vertices, ib, ioff, indices, f, c]) => {
                    let cu = cur.ok_or(-EINVAL)?;
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > indices as u64 || vertices == 0
                    {
                        return Err(-EINVAL);
                    }
                    let iva = self.span(ib, ioff, indices as u64 * 2)?;
                    match cu {
                        Cur::Builtin(k) => {
                            Self::flush(gfx, &mut p, k, &st.uniforms, texture, &mut st.dirty)?;
                            let va = self.span(vb, off, vertices as u64 * k.stride() as u64)?;
                            gfx.draw_indexed(&mut p, k, va, vertices, iva, indices, f, c);
                        }
                        Cur::Custom(pid) => {
                            let pl = self.pipeline(pid)?;
                            if pl.custom.inst_stride != 0 {
                                return Err(-EINVAL);
                            }
                            self.flush_custom(gfx, &mut p, pl, &mut st)?;
                            let lay = Layout {
                                stride: pl.custom.stride,
                                attrs: &pl.custom.attrs,
                                inst_stride: 0,
                            };
                            let va = self.span(vb, off, vertices as u64 * lay.stride as u64)?;
                            gfx.draw_indexed_layout(&mut p, &lay, va, vertices, iva, indices, f, c);
                        }
                    }
                }
                (proto::CMD_DRAW_INSTANCED, &[vb, off, vertices, f, c, ib, ioff, n]) => {
                    let Some(Cur::Custom(pid)) = cur else {
                        return Err(-EINVAL);
                    };
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > vertices as u64 {
                        return Err(-EINVAL);
                    }
                    let pl = self.pipeline(pid)?;
                    let inst = self.instances(pl, p.words.len(), ib, ioff, n)?;
                    self.flush_custom(gfx, &mut p, pl, &mut st)?;
                    let lay = Layout {
                        stride: pl.custom.stride,
                        attrs: &pl.custom.attrs,
                        inst_stride: pl.custom.inst_stride,
                    };
                    let va = self.span(vb, off, vertices as u64 * lay.stride as u64)?;
                    gfx.draw_layout_instanced(&mut p, &lay, va, vertices, f, c, &inst);
                }
                (
                    proto::CMD_DRAW_INDEXED_INSTANCED,
                    &[vb, off, vertices, ib, ioff, indices, f, c, inb, inoff, n],
                ) => {
                    let Some(Cur::Custom(pid)) = cur else {
                        return Err(-EINVAL);
                    };
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > indices as u64 || vertices == 0
                    {
                        return Err(-EINVAL);
                    }
                    let pl = self.pipeline(pid)?;
                    let inst = self.instances(pl, p.words.len(), inb, inoff, n)?;
                    let iva = self.span(ib, ioff, indices as u64 * 2)?;
                    self.flush_custom(gfx, &mut p, pl, &mut st)?;
                    let lay = Layout {
                        stride: pl.custom.stride,
                        attrs: &pl.custom.attrs,
                        inst_stride: pl.custom.inst_stride,
                    };
                    let va = self.span(vb, off, vertices as u64 * lay.stride as u64)?;
                    gfx.draw_indexed_layout_instanced(
                        &mut p, &lay, va, vertices, iva, indices, f, c, &inst,
                    );
                }
                _ => return Err(-EINVAL),
            }
        }
        Ok(p)
    }
}

/// What a stream has bound for the shaders.
#[derive(Default)]
struct Binds {
    bufs: [Option<u32>; proto::SHADER_BUFFERS],
    texs: [Option<u32>; proto::SHADER_TEXTURES],
    dirty: bool,
}

/// The uniforms and bindings of a stream, and whether they changed since
/// the last draw.
#[derive(Default)]
struct Draw {
    uniforms: Vec<u32>,
    dirty: bool,
    binds: Binds,
}
