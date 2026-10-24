//! Ready-made programs games share.

use crate::{Builder, OutKind, Program, Reg, Stage, Ty};

/// Vertex shader of the fullscreen triangle: input 0 is a clip-space
/// (x, y), passed on as the position (z 0, w 1); varying 0 components 0
/// and 1 are the texture coordinates of that corner, x * 0.5 + 0.5 and
/// y * 0.5 + 0.5 (clip y points down the target).
pub fn fullscreen_vertex() -> Program {
    let mut b = Builder::new(Stage::Vertex);
    let (x, y) = (b.input(0, 0), b.input(0, 1));
    let (zero, one, half) = (b.f32(0.0), b.f32(1.0), b.f32(0.5));
    for (c, r) in [x, y, zero, one].into_iter().enumerate() {
        b.output(OutKind::Position, c as u8, r);
    }
    let (u, v) = (b.fma(x, half, half), b.fma(y, half, half));
    b.output(OutKind::Varying(0), 0, u);
    b.output(OutKind::Varying(0), 1, v);
    b.finish()
}

/// Fragment shader of a video pass over the panel (a 720 x 1280 target:
/// the panel shows the landscape picture turned a quarter, so pixel
/// `(px, py)` of the target is column `width - 1 - py`, row `px` of the
/// picture).
/// Varying 0 holds the pixel's (u, v), 0..1 across columns and rows. The
/// planes of a `src_w` x `src_h` I420 picture are bound as buffers 0 (Y,
/// rows of `y_stride` bytes), 1 (U) and 2 (V, rows of `uv_stride` bytes),
/// bytes packed little-endian in words. The nearest sample is taken and
/// converted from BT.709 limited range in 8.8 fixed point, as the desktop
/// client does, so the colours match what it shows.
pub fn yuv420_fragment(src_w: u32, src_h: u32, y_stride: u32, uv_stride: u32) -> Program {
    let mut b = Builder::new(Stage::Fragment);
    let (u, v) = (b.input(0, 0), b.input(0, 1));
    // Picture column from the target's row fraction v, picture row from u.
    let (fw, fh) = (b.f32(src_w as f32), b.f32(src_h as f32));
    // The panel's rows run the picture's columns backwards (seen on the console).
    let vw = b.fmul(v, fw);
    let (cx, cy) = (b.fsub(fw, vw), b.fmul(u, fh));
    let (xi, yi) = (b.f2i(cx), b.f2i(cy));
    let (zero, wmax, hmax) = (b.i32(0), b.i32(src_w as i32 - 1), b.i32(src_h as i32 - 1));
    let (xi, yi) = (b.imax(xi, zero), b.imax(yi, zero));
    let (x, y) = (b.imin(xi, wmax), b.imin(yi, hmax));
    let (x, y) = (b.ubits(x), b.ubits(y));
    let (one, two, three) = (b.u32(1), b.u32(2), b.u32(3));
    let ff = b.u32(0xff);
    // One byte of a plane: word (index >> 2), byte (index & 3).
    let byte = |b: &mut Builder, buf: u8, index: Reg| {
        let word = b.shr(index, two);
        let w = b.load(buf, word, 0, Ty::U32);
        let lane = b.and(index, three);
        let shift = b.shl(lane, three);
        let sh = b.shr(w, shift);
        b.and(sh, ff)
    };
    let ys = b.u32(y_stride);
    let row = b.umul(y, ys);
    let yidx = b.uadd(row, x);
    let yv = byte(&mut b, 0, yidx);
    let (cxx, cyy) = (b.shr(x, one), b.shr(y, one));
    let uvs = b.u32(uv_stride);
    let crow = b.umul(cyy, uvs);
    let cidx = b.uadd(crow, cxx);
    let uv = byte(&mut b, 1, cidx);
    let vv = byte(&mut b, 2, cidx);
    // Fixed point: 298 (Y - 16) plus the chroma terms, rounded, shifted down by 8.
    let (yv, uv, vv) = (b.ibits(yv), b.ibits(uv), b.ibits(vv));
    let (c16, c128) = (b.i32(16), b.i32(128));
    let yy = b.isub(yv, c16);
    let d = b.isub(uv, c128);
    let e = b.isub(vv, c128);
    let k = |b: &mut Builder, n: i32| b.i32(n);
    let c = {
        let k298 = k(&mut b, 298);
        b.imul(yy, k298)
    };
    let (k459, k55, k136, k541) = (
        k(&mut b, 459),
        k(&mut b, -55),
        k(&mut b, -136),
        k(&mut b, 541),
    );
    let e459 = b.imul(e, k459);
    let d55 = b.imul(d, k55);
    let e136 = b.imul(e, k136);
    let d541 = b.imul(d, k541);
    let rnd = b.i32(128);
    let r0 = b.iadd(c, e459);
    let r0 = b.iadd(r0, rnd);
    let g0 = b.iadd(c, d55);
    let g0 = b.iadd(g0, e136);
    let g0 = b.iadd(g0, rnd);
    let b0 = b.iadd(c, d541);
    let b0 = b.iadd(b0, rnd);
    let (sh8, max255) = (b.u32(8), b.i32(255));
    let channel = |b: &mut Builder, x: Reg| {
        let s = b.sar(x, sh8);
        let lo = b.imax(s, zero);
        let hi = b.imin(lo, max255);
        let f = b.i2f(hi);
        let inv = b.f32(1.0 / 255.0);
        b.fmul(f, inv)
    };
    let (r, g, bl) = (
        channel(&mut b, r0),
        channel(&mut b, g0),
        channel(&mut b, b0),
    );
    let a = b.f32(1.0);
    for (comp, reg) in [r, g, bl, a].into_iter().enumerate() {
        b.output(OutKind::Color(0), comp as u8, reg);
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{run, Machine, NoTextures};
    use crate::{lower, validate, MAX_ATTRS};

    #[test]
    fn the_fullscreen_triangle_covers_the_target() {
        let p = fullscreen_vertex();
        validate(&p).expect("valid");
        lower::compile(&p).expect("compiles");
        for ((x, y), (u, v)) in [
            ((-1.0f32, -1.0f32), (0.0f32, 0.0f32)),
            ((3.0, -1.0), (2.0, 0.0)),
            ((-1.0, 3.0), (0.0, 2.0)),
        ] {
            let mut m = Machine {
                inputs: [[0; 4]; MAX_ATTRS],
                builtins: [0; 11],
                uniforms: &[],
                buffers: &mut [],
                sampler: &mut NoTextures,
            };
            m.inputs[0][0] = x.to_bits();
            m.inputs[0][1] = y.to_bits();
            let o = run(&p, &mut m);
            assert_eq!(o.position, [x, y, 0.0, 1.0]);
            assert_eq!(&o.varyings[0][..2], &[u, v]);
        }
    }

    /// The vertex shader `gpu irtest` draws four instances with: offset and
    /// color from attribute 1, the instance number in blue.
    #[test]
    fn an_instanced_vertex_shader_compiles_and_reads_the_instance_number() {
        use crate::{Builtin, Stage};
        let mut b = Builder::new(Stage::Vertex);
        let (x, y) = (b.input(0, 0), b.input(0, 1));
        let (dx, dy) = (b.input(1, 0), b.input(1, 1));
        let (px, py) = (b.fadd(x, dx), b.fadd(y, dy));
        let (z, one, quarter) = (b.f32(0.5), b.f32(1.0), b.f32(0.25));
        for (c, r) in [px, py, z, one].into_iter().enumerate() {
            b.output(OutKind::Position, c as u8, r);
        }
        let id = b.builtin(Builtin::InstanceId);
        let idf = b.u2f(id);
        let blue = b.fmul(idf, quarter);
        for (c, r) in [b.input(1, 2), b.input(1, 3), blue, one]
            .into_iter()
            .enumerate()
        {
            b.output(OutKind::Varying(0), c as u8, r);
        }
        let p = b.finish();
        lower::compile(&p).expect("compiles");
        let mut m = Machine {
            inputs: [[0; 4]; MAX_ATTRS],
            builtins: [0; 11],
            uniforms: &[],
            buffers: &mut [],
            sampler: &mut NoTextures,
        };
        m.builtins[Builtin::InstanceId as usize] = 3;
        m.inputs[1] = [
            (-0.5f32).to_bits(),
            0.5f32.to_bits(),
            1.0f32.to_bits(),
            0.0f32.to_bits(),
        ];
        let o = run(&p, &mut m);
        assert_eq!(o.position, [-0.5, 0.5, 0.5, 1.0]);
        assert_eq!(o.varyings[0], [1.0, 0.0, 0.75, 1.0]);
    }

    /// The desktop client's conversion (sources/luna convert.rs).
    fn reference(y: u8, u: u8, v: u8) -> [u8; 3] {
        let (d, e) = (u as i32 - 128, v as i32 - 128);
        let c = 298 * (y as i32 - 16);
        let ch = |x: i32| ((x >> 8).clamp(0, 255)) as u8;
        [
            ch(c + 459 * e + 128),
            ch(c - 55 * d - 136 * e + 128),
            ch(c + 541 * d + 128),
        ]
    }

    fn pack(bytes: &[u8]) -> Vec<u32> {
        bytes
            .chunks(4)
            .map(|c| {
                c.iter()
                    .enumerate()
                    .fold(0u32, |w, (i, &b)| w | (b as u32) << (8 * i))
            })
            .collect()
    }

    #[test]
    fn the_video_pass_converts_and_turns_the_picture_like_the_desktop_client() {
        use crate::{lower, validate, MAX_BUFFERS};
        let (w, h, ys, uvs) = (96u32, 64u32, 100u32, 52u32);
        let p = yuv420_fragment(w, h, ys, uvs);
        validate(&p).expect("valid");
        lower::compile(&p).expect("compiles to SASS");
        // Planes with strides wider than the picture, filled with a pattern.
        let mut seed = 12345u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        };
        let yp: Vec<u8> = (0..ys * h).map(|_| rnd()).collect();
        let up: Vec<u8> = (0..uvs * h / 2).map(|_| rnd()).collect();
        let vp: Vec<u8> = (0..uvs * h / 2).map(|_| rnd()).collect();
        let mut bufs = vec![pack(&yp), pack(&up), pack(&vp)];
        bufs.resize(MAX_BUFFERS, Vec::new());
        // A 36 x 64 target stands for the panel: 36 columns (picture rows), 64 rows (picture columns).
        let (tw, th) = (36u32, 64u32);
        for py in 0..th {
            for px in 0..tw {
                let (uu, vv) = ((px as f32 + 0.5) / tw as f32, (py as f32 + 0.5) / th as f32);
                let mut m = Machine {
                    inputs: [[0; 4]; MAX_ATTRS],
                    builtins: [0; 11],
                    uniforms: &[],
                    buffers: &mut bufs,
                    sampler: &mut NoTextures,
                };
                m.inputs[0][0] = uu.to_bits();
                m.inputs[0][1] = vv.to_bits();
                let o = run(&p, &mut m);
                let sx = (((1.0 - vv) * w as f32) as u32).min(w - 1);
                let sy = ((uu * h as f32) as u32).min(h - 1);
                let want = reference(
                    yp[(sy * ys + sx) as usize],
                    up[((sy / 2) * uvs + sx / 2) as usize],
                    vp[((sy / 2) * uvs + sx / 2) as usize],
                );
                let got: Vec<u8> = (0..3)
                    .map(|c| (o.colors[0][c] * 255.0).round() as u8)
                    .collect();
                assert_eq!(got, want, "pixel ({}, {})", px, py);
                assert_eq!(o.colors[0][3], 1.0);
            }
        }
    }
}
