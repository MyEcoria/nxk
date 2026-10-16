//! A lit cube turning on the GPU, a pointer-controlled light: the first
//! program on nxgfx. Any button or the time given (10 s) ends it.

use nxgfx::{cube, now_ns, vec3, Event, Graphics, LitUniforms, Mat4, Pipe};

fn main() {
    let mut g = match Graphics::open() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("cube: cannot take the screen: {}", e);
            return;
        }
    };
    let (v, i) = cube(
        1.4,
        [
            [230, 70, 60, 255],
            [60, 150, 230, 255],
            [90, 200, 100, 255],
            [240, 200, 60, 255],
            [180, 90, 220, 255],
            [240, 240, 240, 255],
        ],
    );
    let mesh = match g.mesh_lit(&v, &i) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cube: mesh: errno {}", -e);
            return;
        }
    };
    let ortho = std::env::args().nth(2).is_some();
    let (proj, view) = if ortho {
        (
            Graphics::panel() * Mat4::scale(vec3(0.5 * 9.0 / 16.0, 0.5, 0.2)),
            Mat4::IDENTITY,
        )
    } else {
        (
            Graphics::projection(1.0, 0.1, 50.0),
            Mat4::look_at(
                vec3(0.0, 1.6, 4.5),
                vec3(0.0, 0.0, 0.0),
                vec3(0.0, 1.0, 0.0),
            ),
        )
    };
    let secs: f32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10.0);
    let t0 = now_ns();
    let mut last = t0;
    let mut frames = 0u64;
    loop {
        let t = (now_ns() - t0) as f32 / 1e9;
        if t > secs {
            break;
        }
        while let Some(ev) = g.poll(1) {
            if let Event::Button { down: true, .. } = ev {
                return;
            }
        }
        let model = Mat4::rotation_y(t * 0.9) * Mat4::rotation_x(t * 0.6);
        let u = LitUniforms {
            mvp: proj * view * model,
            model,
            to_light: vec3(0.4, 0.8, 0.6).normalized(),
            ambient: 0.25,
            diffuse: 0.75,
        };
        let mut f = g.frame();
        f.clear([0.05, 0.08, 0.16, 1.0]);
        f.pipeline(Pipe::Lit, false);
        f.lit(&u);
        f.draw(&mesh);
        if f.present().is_err() {
            break;
        }
        frames += 1;
        let now = now_ns();
        if now - last > 2_000_000_000 {
            println!("cube: {} frames in {} ms", frames, (now - last) / 1_000_000);
            frames = 0;
            last = now;
        }
    }
}
