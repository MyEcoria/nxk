//! Ready-made programs games share.

use crate::{Builder, OutKind, Program, Stage};

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
}
