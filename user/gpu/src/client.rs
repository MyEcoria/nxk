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

use crate::gfx::{Color, Depth, Gfx, Pipe, Target, Tiled, LIT_UBO_WORDS, MAX_TEXTURES};
use crate::mmu::{self, Vm, PAGE};
use crate::push::Push;

const MAX_BUFFERS: usize = 64;
const MAX_TARGETS: usize = 16;
/// Pushbuffer words a stream may fill (the pushbuffer holds 64 Ki).
const MAX_WORDS: usize = 48_000;
/// Largest uniform block of a draw, in words.
const MAX_UNIFORM_WORDS: usize = 256;

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
}

struct Tex {
    va: u64,
    bytes: u64,
    _held: Held,
}

#[derive(Default)]
pub struct Client {
    bufs: Vec<Option<Buf>>,
    tgts: Vec<Option<Tgt>>,
    texs: Vec<Option<Tex>>,
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
        vm.map_at(va, &pg, false).map_err(|(e, _)| -e)?;
        self.bufs[i] = Some(Buf { va, len, held });
        Ok(i as u64)
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

    pub fn attach_target(
        &mut self,
        vm: &mut Vm,
        h: u32,
        width: u32,
        height: u32,
        kind: u64,
        block_log2: u32,
    ) -> Res<u64> {
        let held = Held::Raw(h);
        if width == 0 || height == 0 || width > 16384 || height > 16384 || block_log2 > 5 {
            return Err(-EINVAL);
        }
        let (mk, bl, image) = match kind {
            proto::TARGET_COLOR => (
                mmu::KIND_GENERIC_16BX2,
                block_log2,
                Image::Color(Tiled {
                    va: 0,
                    width,
                    height,
                    format: proto::FMT_A8B8G8R8 as u32,
                    block_log2,
                }),
            ),
            proto::TARGET_DEPTH => (
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
        let bytes = proto::tiled_bytes(width, height, bl) as u64;
        let pg = pages(h, bytes)?;
        let i = slot(&mut self.tgts, MAX_TARGETS)?;
        let va = vm.alloc(bytes, 1 << 17).map_err(|(e, _)| -e)?;
        vm.map_at_kind(va, &pg, false, mk).map_err(|(e, _)| -e)?;
        let image = match image {
            Image::Color(c) => Image::Color(Tiled { va, ..c }),
            Image::Depth(d) => Image::Depth(Depth { va, ..d }),
        };
        self.tgts[i] = Some(Tgt {
            image,
            bytes,
            _held: held,
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
    /// error too). The pool entries go in `gfx`.
    pub fn attach_texture(
        &mut self,
        vm: &mut Vm,
        gfx: &Gfx,
        h: u32,
        width: u32,
        height: u32,
        flags: u64,
    ) -> Res<u64> {
        let held = Held::Raw(h);
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || flags & !(proto::TEX_NEAREST | proto::TEX_CLAMP) != 0
        {
            return Err(-EINVAL);
        }
        let bytes = proto::tiled_bytes(width, height, proto::tex_block_log2(height)) as u64;
        let pg = pages(h, bytes)?;
        let i = slot(&mut self.texs, MAX_TEXTURES)?;
        let va = vm.alloc(bytes, 1 << 17).map_err(|(e, _)| -e)?;
        vm.map_at_kind(va, &pg, false, mmu::KIND_GENERIC_16BX2)
            .map_err(|(e, _)| -e)?;
        gfx.set_texture(i, va, width, height, flags);
        self.texs[i] = Some(Tex {
            va,
            bytes,
            _held: held,
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

    /// Everything went: handles and GPU mappings.
    pub fn clear(&mut self, vm: &mut Vm, gfx: &Gfx) {
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

    /// The batch for the stream `first..first + count` of buffer `id`,
    /// or why it is refused; nothing has run.
    pub fn build(&self, gfx: &mut Gfx, id: u32, first: u32, count: u32) -> Res<Push> {
        let w = self.words(id, first, count)?;
        let mut p = Push::default();
        gfx.begin(&mut p);
        let mut at = 0;
        let mut target: Option<Target> = None;
        let mut pipe: Option<Pipe> = None;
        // Uniforms are written before a draw, once they or the texture
        // changed.
        let mut uniforms: Vec<u32> = Vec::new();
        let mut texture: Option<u32> = None;
        let mut dirty = false;
        while at < w.len() {
            let (op, n) = (w[at] & 0xFFFF, (w[at] >> 16) as usize);
            let a = w.get(at + 1..at + 1 + n).ok_or(-EINVAL)?;
            at += 1 + n;
            if p.words.len() > MAX_WORDS {
                return Err(-ENOSPC);
            }
            match (op, a) {
                (proto::CMD_TARGET, &[c, z]) => target = Some(self.target(c, z)?),
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
                    dirty |= pipe != Some(k);
                    pipe = Some(k);
                    gfx.pipeline(&mut p, t, k, flags & proto::PF_CULL_BACK != 0);
                }
                (proto::CMD_UNIFORMS, u) if !u.is_empty() && u.len() <= MAX_UNIFORM_WORDS => {
                    uniforms.clear();
                    uniforms.extend_from_slice(u);
                    dirty = true;
                }
                (proto::CMD_TEXTURE, &[id]) => {
                    let live = self.texs.get(id as usize).is_some_and(|t| t.is_some());
                    if !live {
                        return Err(-EINVAL);
                    }
                    texture = Some(id);
                    dirty = true;
                }
                (proto::CMD_DRAW, &[vb, off, vertices, f, c]) => {
                    let k = pipe.ok_or(-EINVAL)?;
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > vertices as u64 {
                        return Err(-EINVAL);
                    }
                    Self::flush(gfx, &mut p, k, &uniforms, texture, &mut dirty)?;
                    let va = self.span(vb, off, vertices as u64 * k.stride() as u64)?;
                    gfx.draw(&mut p, k, va, vertices, f, c);
                }
                (proto::CMD_DRAW_INDEXED, &[vb, off, vertices, ib, ioff, indices, f, c]) => {
                    let k = pipe.ok_or(-EINVAL)?;
                    if c == 0 || c % 3 != 0 || f as u64 + c as u64 > indices as u64 || vertices == 0
                    {
                        return Err(-EINVAL);
                    }
                    Self::flush(gfx, &mut p, k, &uniforms, texture, &mut dirty)?;
                    let va = self.span(vb, off, vertices as u64 * k.stride() as u64)?;
                    let iva = self.span(ib, ioff, indices as u64 * 2)?;
                    gfx.draw_indexed(&mut p, k, va, vertices, iva, indices, f, c);
                }
                _ => return Err(-EINVAL),
            }
        }
        Ok(p)
    }
}
