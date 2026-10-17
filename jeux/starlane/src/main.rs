//! Starlane: fly a ship down a lane in 3D, dodge the blocks, grab the
//! gems. Controls: D-pad, left stick or L / R to steer, a finger on the screen to
//! steer to it; A, + or a tap to start; B or HOME to leave. Everything is
//! drawn by the GPU through nxgfx: lit meshes with depth, and 2D text.

use std::fs;

use nxgfx::{
    cube, octahedron, rgb, tris_mesh, vec3, Audio, Button, Canvas, Event, Frame, Graphics,
    LitUniforms, Mat4, Mesh, Phase, Rgba, Sampling, Sound, Stick, Texture, Vec3, HEIGHT, WIDTH,
};

/// Half width of the playfield.
const HALF: f32 = 3.9;
const GROUND_Y: f32 = -0.5;
const FAR: f32 = -112.0;
/// World size of one repeat of the ground texture; it divides the
/// scroll's period, so the pattern runs on without a jump.
const GROUND_CELL: f32 = 2.0;
const BEST_FILE: &str = "/games/starlane/best.txt";
/// Columns blocks and gems come in.
const COLUMNS: [f32; 5] = [-3.2, -1.6, 0.0, 1.6, 3.2];

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / (1 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

struct Block {
    x: f32,
    z: f32,
    kind: usize,
}

struct Gem {
    x: f32,
    z: f32,
}

struct Star {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(PartialEq, Clone, Copy)]
enum State {
    Title,
    Playing,
    Over,
}

struct Assets {
    blocks: [Mesh; 3],
    gem: Mesh,
    ship: Mesh,
    flame: Mesh,
    ground: Mesh,
    ground_tex: Texture,
    white: Mesh,
}

/// One cell of the ground: dark violet with a soft neon line along its
/// left and top edges, a little grain.
fn ground_texels() -> Vec<u8> {
    const N: u32 = 64;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            // Distance (texels) to the nearest line; the lines sit on
            // the cell's edge, so they wrap.
            let d = x.min(N - x).min(y).min(N - y) as f32;
            let line = (1.0 - d / 3.0).max(0.0);
            let grain = (x.wrapping_mul(73856093) ^ y.wrapping_mul(19349663)) % 9;
            let base = [46.0, 38.0, 86.0];
            let neon = [150.0, 110.0, 255.0];
            for c in 0..3 {
                let v = base[c] + (neon[c] - base[c]) * line + grain as f32 - 4.0;
                px.push(v.clamp(0.0, 255.0) as u8);
            }
            px.push(255);
        }
    }
    px
}

fn load(g: &Graphics) -> nxgfx::Result<Assets> {
    let solid = |c: [u8; 4]| cube(1.0, [c; 6]);
    let mut blocks = Vec::new();
    for c in [[235, 80, 70, 255], [230, 90, 200, 255], [70, 190, 200, 255]] {
        let (v, i) = solid(c);
        blocks.push(g.mesh_lit(&v, &i)?);
    }
    let (v, i) = octahedron(0.5, [90, 230, 255, 255]);
    let gem = g.mesh_lit(&v, &i)?;
    // Arrowhead, nose toward -z.
    let (n, l, r) = (
        vec3(0.0, 0.05, -1.3),
        vec3(-0.9, -0.05, 0.7),
        vec3(0.9, -0.05, 0.7),
    );
    let (c, d) = (vec3(0.0, 0.38, 0.55), vec3(0.0, -0.14, 0.45));
    let body = [244, 156, 60, 255];
    let wing = [250, 210, 120, 255];
    let under = [150, 90, 50, 255];
    let tris = [
        ([n, l, c], wing),
        ([n, c, r], wing),
        ([c, l, r], body),
        ([n, r, d], under),
        ([n, d, l], under),
        ([d, r, l], under),
    ];
    let (v, i) = tris_mesh(&tris, vec3(0.0, 0.1, 0.0));
    let ship = g.mesh_lit(&v, &i)?;
    let (v, i) = solid([255, 214, 120, 255]);
    let flame = g.mesh_lit(&v, &i)?;
    // Brighter near, fading to the dark of the sky far off.
    let (v, i) = nxgfx::ground_textured(
        HALF + 4.0,
        GROUND_Y,
        FAR,
        14.0,
        1.0 / GROUND_CELL,
        [255, 255, 255, 255],
        [26, 26, 40, 255],
    );
    let ground = g.mesh_textured(&v, &i)?;
    let ground_tex = g.texture(64, 64, &ground_texels(), Sampling::default())?;
    let (v, i) = solid([210, 220, 255, 255]);
    let white = g.mesh_lit(&v, &i)?;
    let [a, b, cc]: [Mesh; 3] = blocks.try_into().ok().unwrap();
    Ok(Assets {
        blocks: [a, b, cc],
        gem,
        ship,
        flame,
        ground,
        ground_tex,
        white,
    })
}

struct Game {
    state: State,
    since: f32,
    ship_x: f32,
    ship_vel: f32,
    target: Option<f32>,
    steer: f32,
    blocks: Vec<Block>,
    gems: Vec<Gem>,
    stars: Vec<Star>,
    next_row: f32,
    speed: f32,
    scroll: f32,
    distance: f32,
    score: u32,
    best: u32,
    shake: f32,
    /// Sound cues since the last frame: gems picked up, a crash.
    gems_got: u32,
    crashed: bool,
    rng: Rng,
}

impl Game {
    fn new(seed: u32) -> Self {
        let mut rng = Rng(seed | 1);
        let stars = (0..40)
            .map(|_| Star {
                x: rng.range(-40.0, 40.0),
                y: rng.range(2.0, 26.0),
                z: rng.range(-110.0, -20.0),
            })
            .collect();
        let best = fs::read_to_string(BEST_FILE)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Game {
            state: State::Title,
            since: 0.0,
            ship_x: 0.0,
            ship_vel: 0.0,
            target: None,
            steer: 0.0,
            blocks: Vec::new(),
            gems: Vec::new(),
            stars,
            next_row: 0.0,
            speed: 16.0,
            scroll: 0.0,
            distance: 0.0,
            score: 0,
            best,
            shake: 0.0,
            gems_got: 0,
            crashed: false,
            rng,
        }
    }

    fn start(&mut self) {
        self.state = State::Playing;
        self.since = 0.0;
        self.blocks.clear();
        self.gems.clear();
        self.ship_x = 0.0;
        self.ship_vel = 0.0;
        self.speed = 16.0;
        self.distance = 0.0;
        self.score = 0;
        self.next_row = 0.0;
        self.shake = 0.0;
    }

    fn spawn_row(&mut self) {
        // Two or three of the five columns blocked, a gem in a free one.
        let mut taken = [false; 5];
        let count = 2 + (self.rng.next() % 2) as usize;
        for _ in 0..count {
            let c = (self.rng.next() % 5) as usize;
            if !taken[c] {
                taken[c] = true;
                let kind = (self.rng.next() % 3) as usize;
                self.blocks.push(Block {
                    x: COLUMNS[c],
                    z: FAR + 10.0,
                    kind,
                });
            }
        }
        if self.rng.unit() < 0.55 {
            let free: Vec<usize> = (0..5).filter(|&c| !taken[c]).collect();
            if !free.is_empty() {
                let c = free[(self.rng.next() as usize) % free.len()];
                self.gems.push(Gem {
                    x: COLUMNS[c],
                    z: FAR + 10.0 - 3.0,
                });
            }
        }
    }

    fn update(&mut self, dt: f32) {
        self.since += dt;
        // Steering: a finger goes to its place, the D-pad pushes.
        let want = match self.target {
            Some(x) => x,
            None => self.ship_x + self.steer * 6.0,
        };
        let want = want.clamp(-HALF + 0.5, HALF - 0.5);
        let accel = if self.target.is_some() { 80.0 } else { 70.0 };
        let delta = want - self.ship_x;
        self.ship_vel += (delta * accel - self.ship_vel * 12.0) * dt;
        self.ship_vel = self.ship_vel.clamp(-14.0, 14.0);
        self.ship_x = (self.ship_x + self.ship_vel * dt).clamp(-HALF + 0.5, HALF - 0.5);

        let v = if self.state == State::Playing {
            self.speed
        } else {
            6.0
        };
        self.scroll = (self.scroll + v * dt) % 8.0;
        for s in &mut self.stars {
            s.z += v * 0.15 * dt;
            if s.z > -8.0 {
                s.z = -110.0;
                s.x = self.rng.range(-40.0, 40.0);
                s.y = self.rng.range(2.0, 26.0);
            }
        }
        if self.shake > 0.0 {
            self.shake = (self.shake - dt).max(0.0);
        }
        if self.state != State::Playing {
            return;
        }
        self.speed = (16.0 + self.distance * 0.0045).min(44.0);
        self.distance += self.speed * dt;
        self.next_row -= self.speed * dt;
        if self.next_row <= 0.0 {
            self.spawn_row();
            self.next_row = 15.0 - (self.speed - 16.0) * 0.18;
        }
        for b in &mut self.blocks {
            b.z += self.speed * dt;
        }
        for g in &mut self.gems {
            g.z += self.speed * dt;
        }
        self.blocks.retain(|b| b.z < 9.0);
        self.gems.retain(|g| g.z < 9.0);
        // Gems picked up, blocks hit.
        let sx = self.ship_x;
        let before = self.gems.len();
        self.gems
            .retain(|g| !((g.x - sx).abs() < 0.9 && g.z.abs() < 1.2));
        self.gems_got += (before - self.gems.len()) as u32;
        self.score += 250 * (before - self.gems.len()) as u32;
        let hit = self
            .blocks
            .iter()
            .any(|b| (b.x - sx).abs() < 0.45 + 0.6 && b.z.abs() < 0.6 + 0.8);
        let dist = (self.distance * 0.1) as u32;
        self.score = self.score.max(dist);
        if hit {
            self.state = State::Over;
            self.crashed = true;
            self.since = 0.0;
            self.shake = 0.6;
            if self.score > self.best {
                self.best = self.score;
                let _ = fs::write(BEST_FILE, format!("{}\n", self.best));
            }
        }
    }
}

fn draw_obj(f: &mut Frame, mesh: &Mesh, vp: &Mat4, pos: Vec3, rot: Mat4, scale: Vec3) {
    let model = Mat4::translation(pos) * rot * Mat4::scale(scale);
    let u = LitUniforms {
        mvp: *vp * model,
        model: rot,
        to_light: vec3(-0.35, 0.85, 0.45).normalized(),
        ambient: 0.34,
        diffuse: 0.66,
    };
    f.lit(&u);
    f.draw(mesh);
}

const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];

fn hud(c: &mut Canvas, game: &Game, t: f32) {
    let dim: Rgba = [0.75, 0.82, 1.0, 0.9];
    c.text(36.0, 28.0, 4.0, &format!("SCORE {:06}", game.score), WHITE);
    let best = format!("BEST {:06}", game.best);
    c.text(
        WIDTH as f32 - 36.0 - nxgfx::canvas::text_width(best.len(), 4.0),
        28.0,
        4.0,
        &best,
        dim,
    );
    match game.state {
        State::Title => {
            c.text_center(
                WIDTH as f32 / 2.0,
                190.0,
                14.0,
                "STARLANE",
                rgb(255, 200, 100),
            );
            let a = 0.55 + 0.45 * nxgfx::math::sin_cos(t * 3.0).0;
            c.text_center(
                WIDTH as f32 / 2.0,
                430.0,
                4.0,
                "PRESS A OR TOUCH TO START",
                [a, a, a, 1.0],
            );
            c.text_center(
                WIDTH as f32 / 2.0,
                640.0,
                3.0,
                "D-PAD OR DRAG TO STEER  B TO QUIT",
                dim,
            );
        }
        State::Over => {
            c.text_center(
                WIDTH as f32 / 2.0,
                230.0,
                12.0,
                "CRASHED",
                rgb(255, 110, 100),
            );
            c.text_center(
                WIDTH as f32 / 2.0,
                360.0,
                5.0,
                &format!("SCORE {}", game.score),
                WHITE,
            );
            if game.since > 0.8 {
                let a = 0.55 + 0.45 * nxgfx::math::sin_cos(t * 3.0).0;
                c.text_center(
                    WIDTH as f32 / 2.0,
                    450.0,
                    4.0,
                    "PRESS A OR TOUCH TO FLY AGAIN",
                    [a, a, a, 1.0],
                );
            }
        }
        State::Playing => {}
    }
}

fn main() {
    let mut g = match Graphics::open() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("starlane: cannot take the screen: {}", e);
            return;
        }
    };
    let assets = match load(&g) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("starlane: assets: errno {}", -e);
            return;
        }
    };
    // Sound is a bonus: no audio service (the emulator) or no codec, no sound.
    let mut audio = Audio::open().ok();
    let (sfx_gem, sfx_start, sfx_crash) = (
        Sound::tone(660.0, 1320.0, 140, 0.5, true),
        Sound::tone(220.0, 660.0, 260, 0.5, true),
        Sound::noise(700, 0.8),
    );
    let proj = Graphics::projection(1.0, 0.2, 140.0);
    let mut game = Game::new(g.seconds().to_bits() ^ 0x9E37_79B9);
    let (mut left, mut right) = (false, false);
    let mut analog = 0.0f32;
    let mut last = g.seconds();
    let (mut fps_n, mut fps_t) = (0u32, last);
    let mut bg = Canvas::background();
    let mut front = Canvas::new();
    loop {
        let now = g.seconds();
        let dt = (now - last).clamp(0.001, 0.05);
        last = now;
        // Input.
        let mut go = false;
        let mut quit = false;
        while let Some(ev) = g.poll(1) {
            match ev {
                Event::Button { button, down } => match button {
                    Button::Left | Button::L | Button::Zl => left = down,
                    Button::Right | Button::R | Button::Zr => right = down,
                    Button::A | Button::Plus if down => go = true,
                    Button::B if down && game.state != State::Playing => quit = true,
                    Button::Home if down => quit = true,
                    _ => {}
                },
                Event::Stick {
                    stick: Stick::Left,
                    x,
                    ..
                } => analog = x,
                Event::Stick { .. } => {}
                Event::Touch { phase, x, .. } => match phase {
                    Phase::Down => {
                        go = true;
                        game.target = Some(((x as f32 / WIDTH as f32) * 2.0 - 1.0) * HALF * 1.25);
                    }
                    Phase::Move => {
                        game.target = Some(((x as f32 / WIDTH as f32) * 2.0 - 1.0) * HALF * 1.25)
                    }
                    Phase::Up => game.target = None,
                },
            }
        }
        if quit {
            break;
        }
        game.steer = ((right as i32 - left as i32) as f32 + analog).clamp(-1.0, 1.0);
        if go && game.state != State::Playing && (game.state == State::Title || game.since > 0.8) {
            game.start();
            if let Some(a) = audio.as_mut() {
                a.play(&sfx_start, 0.4);
            }
        }
        game.update(dt);
        if let Some(a) = audio.as_mut() {
            if game.gems_got > 0 {
                a.play(&sfx_gem, 0.35);
            }
            if game.crashed {
                a.play(&sfx_crash, 0.45);
            }
            a.pump();
        }
        game.gems_got = 0;
        game.crashed = false;

        // Scene.
        let shake = if game.shake > 0.0 {
            vec3(game.rng.range(-1.0, 1.0), game.rng.range(-1.0, 1.0), 0.0) * (game.shake * 0.25)
        } else {
            Vec3::default()
        };
        let eye = vec3(game.ship_x * 0.35, 3.3, 7.4) + shake;
        let view = Mat4::look_at(
            eye,
            vec3(game.ship_x * 0.25, 0.5, -12.0),
            vec3(0.0, 1.0, 0.0),
        );
        let vp = proj * view;
        bg.clear();
        bg.gradient(
            0.0,
            0.0,
            WIDTH as f32,
            330.0,
            rgb(8, 10, 34),
            rgb(46, 30, 92),
        );
        bg.gradient(
            0.0,
            330.0,
            WIDTH as f32,
            80.0,
            rgb(46, 30, 92),
            rgb(120, 60, 120),
        );
        bg.rect(
            0.0,
            410.0,
            WIDTH as f32,
            HEIGHT as f32 - 410.0,
            rgb(20, 16, 40),
        );
        front.clear();
        hud(&mut front, &game, now);

        let mut f = g.frame();
        f.clear([0.03, 0.04, 0.12, 1.0]);
        f.canvas(&bg);
        let id = Mat4::IDENTITY;
        let one = vec3(1.0, 1.0, 1.0);
        // The ground moves a cell at most: the pattern scrolls with the lane.
        f.pipeline(nxgfx::Pipe::Textured, false);
        f.texture(&assets.ground_tex);
        let slide = vec3(0.0, 0.0, game.scroll % GROUND_CELL);
        draw_obj(&mut f, &assets.ground, &vp, slide, id, one);
        f.pipeline(nxgfx::Pipe::Lit, false);
        for s in &game.stars {
            draw_obj(
                &mut f,
                &assets.white,
                &vp,
                vec3(s.x, s.y, s.z),
                id,
                one * 0.18,
            );
        }
        for k in 0..16 {
            let z = 6.0 - k as f32 * 8.0 + game.scroll;
            for x in [-1.6, 1.6] {
                draw_obj(
                    &mut f,
                    &assets.white,
                    &vp,
                    vec3(x, GROUND_Y + 0.02, z),
                    id,
                    vec3(0.12, 0.03, 2.2),
                );
            }
            for x in [-HALF - 0.1, HALF + 0.1] {
                draw_obj(
                    &mut f,
                    &assets.blocks[2],
                    &vp,
                    vec3(x, GROUND_Y + 0.15, z),
                    id,
                    vec3(0.2, 0.3, 3.0),
                );
            }
        }
        for b in &game.blocks {
            draw_obj(
                &mut f,
                &assets.blocks[b.kind],
                &vp,
                vec3(b.x, GROUND_Y + 0.55, b.z),
                id,
                one * 1.1,
            );
        }
        for (k, gem) in game.gems.iter().enumerate() {
            let spin = Mat4::rotation_y(now * 2.5 + k as f32);
            let bob = nxgfx::math::sin_cos(now * 3.0 + k as f32).0 * 0.12;
            draw_obj(
                &mut f,
                &assets.gem,
                &vp,
                vec3(gem.x, GROUND_Y + 0.8 + bob, gem.z),
                spin,
                one * 1.1,
            );
        }
        if game.state != State::Over {
            let bob = nxgfx::math::sin_cos(now * 4.0).0 * 0.04;
            let roll = Mat4::rotation_z(-game.ship_vel * 0.045);
            let pos = vec3(game.ship_x, GROUND_Y + 0.55 + bob, 0.0);
            draw_obj(&mut f, &assets.ship, &vp, pos, roll, one * 0.9);
            let flicker = 0.7 + 0.3 * nxgfx::math::sin_cos(now * 40.0).0;
            draw_obj(
                &mut f,
                &assets.flame,
                &vp,
                pos + vec3(0.0, 0.0, 0.95),
                roll,
                vec3(0.28, 0.2, 0.5 * flicker),
            );
        }
        f.canvas(&front);
        if f.present().is_err() {
            break;
        }
        fps_n += 1;
        if now - fps_t >= 5.0 {
            println!("starlane: {} fps", fps_n as f32 / (now - fps_t));
            fps_n = 0;
            fps_t = now;
        }
    }
}
