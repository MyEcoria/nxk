//! Lumen: run through a valley from dawn to night and collect the lights.
//! Left stick moves, right stick turns the camera, A jumps, R / ZR runs,
//! HOME leaves. Touch: the left half of the screen is a stick, drag the
//! right half to turn.
//!
//! Hidden: L + R + Minus opens the Lab (see `lab`), which tests the GPU
//! with what 3D games need and shows the numbers.
//!
//! Everything solid is drawn by the game's own shaders (`shaders`, in the
//! IR the console compiles for the GPU): a sun and a hemisphere light that
//! follow the time of day, distance fog, gamma.

mod lab;
mod light;
mod meshes;
mod player;
mod shaders;
mod world;

use nxgfx::raytrace::Triangle;
use nxgfx::{
    rgb, vec3, Attr, Blend, Button, Canvas, Cull, Event, Frame, Graphics, Mat4, Mesh, Phase,
    Pipeline, PipelineDesc, Sound, Stick, Audio, HEIGHT, WIDTH,
};

use lab::Lab;
use meshes::MeshData;
use shaders::*;

struct Pipes {
    lit: Pipeline,
    alpha: Pipeline,
    add: Pipeline,
}

struct Assets {
    pipes: Pipes,
    terrain: Mesh,
    trees: Mesh,
    rocks: Mesh,
    water: Mesh,
    cube: Mesh,
    orb: Mesh,
    torso: Mesh,
    head: Mesh,
    arm: Mesh,
    leg: Mesh,
}

fn mesh(g: &Graphics, m: &MeshData) -> nxgfx::Result<Mesh> {
    g.mesh_raw(&m.v, &m.i)
}

fn load(g: &Graphics, terrain: &MeshData, scenery: &world::Scenery) -> nxgfx::Result<Assets> {
    let vs = g.shader(&vertex())?;
    let fs = g.shader(&fragment())?;
    let attrs = [(0, Attr::F32x3), (12, Attr::F32x3), (24, Attr::U8x4Unorm)];
    let desc = |cull, blend| {
        let mut d = PipelineDesc::new(&vs, &fs, meshes::STRIDE, &attrs);
        d.cull = cull;
        d.blend = blend;
        d.depth_write = blend == Blend::None;
        d
    };
    let pipes = Pipes {
        lit: g.pipeline_from(&desc(Cull::Back, Blend::None))?,
        alpha: g.pipeline_from(&desc(Cull::None, Blend::Alpha))?,
        add: g.pipeline_from(&desc(Cull::None, Blend::Additive))?,
    };
    let mut water = MeshData::new();
    let h = world::HALF * 1.2;
    water.quad(
        [-h, world::WATER, h],
        [h, world::WATER, h],
        [h, world::WATER, -h],
        [-h, world::WATER, -h],
        [40, 96, 150, 150],
    );
    let mut cube = MeshData::new();
    cube.cuboid([-0.5; 3], [0.5; 3], [255, 255, 255, 255]);
    let mut orb = MeshData::new();
    orb.octa([0.0; 3], 0.45, [255, 255, 255, 255]);
    let fig = player::figure();
    Ok(Assets {
        pipes,
        terrain: mesh(g, terrain)?,
        trees: mesh(g, &scenery.trees)?,
        rocks: mesh(g, &scenery.rocks)?,
        water: mesh(g, &water)?,
        cube: mesh(g, &cube)?,
        orb: mesh(g, &orb)?,
        torso: mesh(g, &fig.torso)?,
        head: mesh(g, &fig.head)?,
        arm: mesh(g, &fig.arm)?,
        leg: mesh(g, &fig.leg)?,
    })
}

/// The shaders' uniform block and what a frame counts.
struct Draws {
    u: [f32; WORDS],
    draws: u32,
    tris: u32,
}

impl Draws {
    fn new(sky: &light::Sky, cam: [f32; 3]) -> Self {
        let mut u = [0.0f32; WORDS];
        let set3 = |u: &mut [f32; WORDS], at: u16, v: [f32; 3]| {
            u[at as usize..at as usize + 3].copy_from_slice(&v);
        };
        set3(&mut u, U_SUN_DIR, sky.sun_dir);
        set3(&mut u, U_SUN_COL, sky.sun_col);
        set3(&mut u, U_SKY, sky.ambient_sky);
        set3(&mut u, U_GROUND, sky.ambient_ground);
        set3(&mut u, U_FOG, sky.fog);
        set3(&mut u, U_CAMERA, cam);
        u[U_FOG_DENSITY as usize] = 0.0055;
        Self {
            u,
            draws: 0,
            tris: 0,
        }
    }

    fn draw(&mut self, f: &mut Frame, m: &Mesh, vp: &Mat4, model: &Mat4, tint: [f32; 4], emissive: f32) {
        let mvp = *vp * *model;
        for r in 0..4 {
            for c in 0..4 {
                self.u[U_MVP as usize + 4 * r + c] = mvp.0[r][c];
            }
        }
        for r in 0..3 {
            for c in 0..4 {
                self.u[U_MODEL as usize + 4 * r + c] = model.0[r][c];
            }
        }
        self.u[U_TINT as usize..U_TINT as usize + 4].copy_from_slice(&tint);
        self.u[U_EMISSIVE as usize] = emissive;
        f.uniforms_f32(&self.u);
        f.draw(m);
        self.draws += 1;
        self.tris += m.index_count / 3;
    }
}

fn gamma(c: [f32; 3]) -> [f32; 4] {
    [c[0].max(0.0).sqrt(), c[1].max(0.0).sqrt(), c[2].max(0.0).sqrt(), 1.0]
}

#[derive(Default)]
struct Input {
    l: bool,
    r: bool,
    zr: bool,
    mv: [f32; 2],
    look: [f32; 2],
    touch_move: Option<(i32, i32)>,
    touch_look: Option<i32>,
}

fn main() {
    let mut g = match Graphics::open() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("lumen: cannot take the screen: {}", e);
            return;
        }
    };
    let terrain = world::Terrain::new();
    let tmesh = terrain.mesh();
    let scenery = world::scatter(&terrain, 0x1D2C_3B4A);
    let assets = match load(&g, &tmesh, &scenery) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lumen: assets: errno {}", -e);
            return;
        }
    };
    let mut audio = Audio::open().ok();
    let chime = Sound::tone(880.0, 1760.0, 180, 0.5, true);
    let proj = Graphics::projection(1.0, 0.3, 400.0);
    let mut me = player::Player::new(&terrain);
    let mut collected = vec![false; scenery.orbs.len()];
    let mut got = 0usize;
    let (mut yaw, mut pitch) = (0.0f32, 0.35f32);
    let mut tod = 0.27f32;
    let mut time_rate = 1.0 / 300.0;
    let mut inp = Input::default();
    let mut lab_mode: Option<Lab> = None;
    let mut world_tris: Option<Vec<Triangle>> = None;
    let mut last = g.seconds();
    let started = last;
    let mut bg = Canvas::background();
    let mut hud = Canvas::new();
    loop {
        let now = g.seconds();
        let dt = (now - last).clamp(0.001, 0.05);
        last = now;
        // Input.
        let mut quit = false;
        let mut jump = false;
        while let Some(ev) = g.poll(1) {
            match ev {
                Event::Button { button, down } => {
                    match button {
                        Button::L => inp.l = down,
                        Button::R => inp.r = down,
                        Button::Zr => inp.zr = down,
                        _ => {}
                    }
                    if !down {
                        continue;
                    }
                    match button {
                        Button::Home => quit = true,
                        Button::Minus if inp.l && inp.r => {
                            lab_mode = match lab_mode {
                                Some(_) => None,
                                None => Some(Lab::new()),
                            };
                        }
                        Button::A => jump = true,
                        Button::X => tod = (tod + 0.04) % 1.0,
                        Button::Y => time_rate = if time_rate > 0.0 { 0.0 } else { 1.0 / 300.0 },
                        b => {
                            if let Some(l) = lab_mode.as_mut() {
                                l.button(b);
                            }
                        }
                    }
                }
                Event::Stick { stick: Stick::Left, x, y } => inp.mv = [x, y],
                Event::Stick { stick: Stick::Right, x, y } => inp.look = [x, y],
                Event::Touch { phase, x, y, .. } => match phase {
                    Phase::Down => {
                        if x < WIDTH as i32 / 2 {
                            inp.touch_move = Some((x, y));
                        } else {
                            inp.touch_look = Some(x);
                        }
                    }
                    Phase::Move => {
                        if let Some((ox, oy)) = inp.touch_move {
                            if x < WIDTH as i32 / 2 + 80 {
                                inp.mv = [
                                    ((x - ox) as f32 / 90.0).clamp(-1.0, 1.0),
                                    (-(y - oy) as f32 / 90.0).clamp(-1.0, 1.0),
                                ];
                            }
                        }
                        if let Some(lx) = inp.touch_look {
                            if x >= WIDTH as i32 / 2 {
                                yaw -= (x - lx) as f32 * 0.006;
                                inp.touch_look = Some(x);
                            }
                        }
                    }
                    Phase::Up => {
                        if x < WIDTH as i32 / 2 {
                            inp.touch_move = None;
                            inp.mv = [0.0, 0.0];
                        } else {
                            inp.touch_look = None;
                        }
                    }
                },
            }
        }
        if quit {
            break;
        }
        yaw -= inp.look[0] * 2.4 * dt;
        pitch = (pitch - inp.look[1] * 1.5 * dt).clamp(-0.15, 1.25);
        tod = (tod + time_rate * dt) % 1.0;

        // The hero.
        let fwd = [-yaw.sin(), -yaw.cos()];
        let right = [yaw.cos(), -yaw.sin()];
        let wish = [
            fwd[0] * inp.mv[1] + right[0] * inp.mv[0],
            fwd[1] * inp.mv[1] + right[1] * inp.mv[0],
        ];
        me.update(&terrain, dt, wish, inp.r || inp.zr, jump);
        for (k, o) in scenery.orbs.iter().enumerate() {
            if collected[k] {
                continue;
            }
            let d = [o[0] - me.pos[0], o[1] - (me.pos[1] + 1.0), o[2] - me.pos[2]];
            if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < 1.6 * 1.6 {
                collected[k] = true;
                got += 1;
                if let Some(a) = audio.as_mut() {
                    a.play(&chime, 0.4);
                }
            }
        }
        if let Some(a) = audio.as_mut() {
            a.pump();
        }

        // Camera: behind and above, above the ground.
        let target = vec3(me.pos[0], me.pos[1] + 1.4, me.pos[2]);
        let dist = 6.5;
        let mut eye = [
            target.x + yaw.sin() * pitch.cos() * dist,
            target.y + pitch.sin() * dist,
            target.z + yaw.cos() * pitch.cos() * dist,
        ];
        eye[1] = eye[1].max(terrain.height(eye[0], eye[2]) + 0.5);
        let view = Mat4::look_at(vec3(eye[0], eye[1], eye[2]), target, vec3(0.0, 1.0, 0.0));
        let vp = proj * view;
        let sky = light::sky(tod);

        // The scene.
        bg.clear();
        let top = gamma(sky.top);
        let hor = gamma(sky.horizon);
        bg.gradient(0.0, 0.0, WIDTH as f32, HEIGHT as f32 * 0.62, top, hor);
        bg.rect(0.0, HEIGHT as f32 * 0.62, WIDTH as f32, HEIGHT as f32, hor);
        let mut d = Draws::new(&sky, eye);
        // The Lab's resources, made before the frame borrows the screen.
        if let Some(lab) = lab_mode.as_mut() {
            match lab.room {
                2 => lab.ensure_particles(&g),
                3 if lab.rays.is_none() => {
                    let tris = world_tris.get_or_insert_with(|| {
                        let mut t = tmesh.triangles(0);
                        let n = t.len() as u32;
                        t.extend(scenery.trees.triangles(n));
                        let n = t.len() as u32;
                        t.extend(scenery.rocks.triangles(n));
                        t
                    });
                    lab.ensure_rays(&g, tris);
                }
                _ => {}
            }
        }
        let mut f = g.frame();
        f.clear(hor);
        f.canvas(&bg);
        f.use_pipeline(&assets.pipes.lit);
        let id = Mat4::IDENTITY;
        let white = [1.0; 4];
        d.draw(&mut f, &assets.terrain, &vp, &id, white, 0.0);
        d.draw(&mut f, &assets.trees, &vp, &id, white, 0.0);
        d.draw(&mut f, &assets.rocks, &vp, &id, white, 0.0);
        for (k, o) in scenery.orbs.iter().enumerate() {
            if collected[k] {
                continue;
            }
            let t = now + k as f32;
            let m = Mat4::translation(vec3(o[0], o[1] + t.sin() * 0.15, o[2])) * Mat4::rotation_y(t * 1.7);
            d.draw(&mut f, &assets.orb, &vp, &m, [1.0, 0.85, 0.35, 1.0], 1.4);
        }
        let pose = player::pose(&me);
        for (m, part) in pose.iter().zip([
            &assets.torso,
            &assets.head,
            &assets.arm,
            &assets.arm,
            &assets.leg,
            &assets.leg,
        ]) {
            d.draw(&mut f, part, &vp, m, white, 0.0);
        }

        // The Lab's extra loads.
        let mut hud_lab = false;
        if let Some(lab) = lab_mode.as_mut() {
            hud_lab = true;
            let fwd3 = {
                let to = [target.x - eye[0], target.y - eye[1], target.z - eye[2]];
                meshes::normalize(to)
            };
            let right3 = meshes::normalize(meshes::cross(fwd3, [0.0, 1.0, 0.0]));
            let up3 = meshes::cross(right3, fwd3);
            match lab.room {
                1 => {
                    let n = lab.cubes;
                    for k in 0..n {
                        let (i, j, l) = (k % 10, (k / 10) % 10, k / 100);
                        let p = [
                            me.pos[0] + (i as f32 - 4.5) * 1.6,
                            me.pos[1] + 6.0 + j as f32 * 1.6,
                            me.pos[2] - 8.0 - l as f32 * 3.0,
                        ];
                        let m = Mat4::translation(vec3(p[0], p[1], p[2])) * Mat4::rotation_y(now + k as f32 * 0.3);
                        let hue = k as f32 * 0.37;
                        let c = [
                            0.5 + 0.5 * hue.sin(),
                            0.5 + 0.5 * (hue + 2.1).sin(),
                            0.5 + 0.5 * (hue + 4.2).sin(),
                            1.0,
                        ];
                        d.draw(&mut f, &assets.cube, &vp, &m, c, 0.2);
                    }
                }
                2 => {
                    if let Some((vb, ib)) = lab.particle_buf.as_ref() {
                        let verts = lab.particle_vertices(now, me.pos, right3, up3);
                        let words = unsafe {
                            std::slice::from_raw_parts(
                                verts.as_ptr() as *const u32,
                                verts.len() * meshes::STRIDE as usize / 4,
                            )
                        };
                        vb.write_words(0, words);
                        f.use_pipeline(&assets.pipes.add);
                        d.u[U_TINT as usize..U_TINT as usize + 4].copy_from_slice(&[1.0; 4]);
                        for r in 0..3 {
                            for c in 0..4 {
                                d.u[U_MODEL as usize + 4 * r + c] = id.0[r][c];
                            }
                        }
                        for r in 0..4 {
                            for c in 0..4 {
                                d.u[U_MVP as usize + 4 * r + c] = vp.0[r][c];
                            }
                        }
                        d.u[U_EMISSIVE as usize] = 1.5;
                        f.uniforms_f32(&d.u);
                        f.draw_buffers(vb, lab.particles * 4, ib, lab.particles * 6);
                        d.draws += 1;
                        d.tris += lab.particles * 2;
                    }
                }
                3 => {
                    if let Some(r) = lab.rays.as_mut() {
                        r.trace(&mut f, eye, fwd3, right3, up3, sky.sun_dir);
                    }
                }
                _ => {}
            }
        }
        f.use_pipeline(&assets.pipes.alpha);
        d.draw(&mut f, &assets.water, &vp, &id, white, 0.0);

        // The HUD.
        hud.clear();
        if let Some(lab) = lab_mode.as_mut() {
            lab.draws = d.draws;
            lab.tris = d.tris;
            lab.frame_time(dt * 1000.0);
            lab.hud(&mut hud);
            if let Some(r) = lab.rays.as_ref() {
                if lab.room == 3 {
                    r.draw(&mut hud);
                }
            }
        }
        if !hud_lab {
            hud_text(&mut hud, got, collected.len(), tod, now - started);
        }
        f.canvas(&hud);
        if f.present().is_err() {
            match lab_mode.as_mut() {
                // A load too big for a frame: the Lab backs off.
                Some(lab) => {
                    lab.refused += 1;
                    lab.cubes = (lab.cubes / 2).max(50);
                }
                None => break,
            }
        } else if let Some(lab) = lab_mode.as_mut() {
            if lab.room == 3 {
                if let Some(r) = lab.rays.as_mut() {
                    r.collect();
                }
            }
        }
    }
}

fn hud_text(c: &mut Canvas, got: usize, total: usize, tod: f32, since_start: f32) {
    let hours = tod * 24.0;
    let (h, m) = (hours as u32 % 24, ((hours.fract()) * 60.0) as u32);
    c.text(24.0, 20.0, 3.0, &format!("LIGHTS {}/{}", got, total), rgb(255, 214, 120));
    c.text(24.0, 52.0, 2.0, &format!("{:02}:{:02}", h, m), rgb(220, 230, 255));
    if since_start < 8.0 {
        c.text_center(
            WIDTH as f32 / 2.0,
            HEIGHT as f32 - 56.0,
            2.0,
            "LEFT STICK MOVE   RIGHT STICK CAMERA   A JUMP   R RUN   HOME QUIT",
            rgb(255, 255, 255),
        );
    }
    if got == total {
        c.text_center(
            WIDTH as f32 / 2.0,
            HEIGHT as f32 / 2.0 - 40.0,
            5.0,
            "ALL THE LIGHTS ARE BACK",
            rgb(255, 230, 150),
        );
    }
}
