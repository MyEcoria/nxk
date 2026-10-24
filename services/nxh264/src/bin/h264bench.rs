//! `h264bench FILE.h264 [LOOPS [THREADS]]`: decodes an Annex B file and prints the time
//! a frame takes (on the console: `run /bin/h264bench`, file on the card).

use std::time::Instant;

fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let (mut aus, mut cur, mut pending, mut has_slice) =
        (Vec::new(), Vec::new(), Vec::new(), false);
    for nal in nxh264::nal_units(data) {
        let mut bytes = vec![0, 0, 0, 1];
        bytes.extend_from_slice(nal);
        if matches!(nal[0] & 31, 1 | 5) {
            if nal.get(1).is_some_and(|b| b & 0x80 != 0) && has_slice {
                aus.push(std::mem::take(&mut cur));
            }
            cur.append(&mut pending);
            cur.extend_from_slice(&bytes);
            has_slice = true;
        } else if has_slice {
            pending.extend_from_slice(&bytes);
        } else {
            cur.extend_from_slice(&bytes);
        }
    }
    aus.push(cur);
    aus
}

/// `h264bench mc`: the luma interpolation alone, a 16x16 block in cache, by vector fraction.
fn mc_bench() {
    let mut plane = nxh264::pic::Picture::new(16, 16).y;
    for (i, v) in plane.data.iter_mut().enumerate() {
        *v = (i * 7 + i / 256 * 13) as u8;
    }
    let mut out = vec![0u8; 16 * 16];
    println!("h264bench: 16x16 luma block, ns each, by (fx, fy) in quarter samples:");
    for fy in 0..4i16 {
        let mut line = String::new();
        for fx in 0..4i16 {
            let n = 20000;
            let t = Instant::now();
            for k in 0..n {
                let mv = [fx + 4 * (k % 8) as i16, fy + 4 * (k % 5) as i16];
                nxh264::inter::luma(&plane, 64, 64, 16, 16, mv, &mut out, 0, 16);
                std::hint::black_box(&out);
            }
            line.push_str(&format!(
                " {:6.0}",
                t.elapsed().as_nanos() as f64 / n as f64
            ));
        }
        println!("h264bench: {}", line);
    }
}

/// `h264bench mem`: how fast memory is for a program: copy and sum of a buffer
/// that fits the L1 cache, then one that does not.
fn mem_bench() {
    for kb in [8usize, 64, 1024, 8192] {
        let n = kb * 1024;
        let src = vec![1u8; n];
        let mut dst = vec![0u8; n];
        let loops = (256 * 1024 * 1024 / n).max(4);
        let t = Instant::now();
        for _ in 0..loops {
            dst.copy_from_slice(&src);
            std::hint::black_box(&dst);
        }
        let copy = (loops * n) as f64 / t.elapsed().as_secs_f64() / 1e6;
        let t = Instant::now();
        let mut sum = 0u64;
        for _ in 0..loops {
            sum += src.iter().map(|&b| b as u64).sum::<u64>();
        }
        std::hint::black_box(sum);
        let read = (loops * n) as f64 / t.elapsed().as_secs_f64() / 1e6;
        println!(
            "h264bench: {:>5} KB: copy {:7.0} MB/s, sum {:7.0} MB/s",
            kb, copy, read
        );
    }
}

/// `h264bench selftest`: the platform's luma interpolation (NEON on the console) against the
/// portable one, on every block size, fraction and edge case.
fn selftest() -> bool {
    let mut p = nxh264::pic::Picture::new(5, 4).y;
    let mut seed = 77u32;
    for v in p.data.iter_mut() {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *v = (seed >> 24) as u8;
    }
    let mut bad = 0;
    for &(w, h) in &[
        (16usize, 16usize),
        (16, 8),
        (8, 16),
        (8, 8),
        (8, 4),
        (4, 8),
        (4, 4),
    ] {
        for fy in 0..4i16 {
            for fx in 0..4i16 {
                for &(x, y, dx, dy) in &[
                    (32usize, 16usize, 0i16, 0i16),
                    (0, 0, 3, -5),
                    (64, 48, 9, 7),
                    (72, 56, -2, 3),
                    (4, 4, -40, -30),
                    (40, 30, 50, 40),
                    (16, 0, -3, -2),
                ] {
                    let mv = [dx * 4 + fx, dy * 4 + fy];
                    let (mut a, mut b) = (vec![9u8; 16 * 16], vec![9u8; 16 * 16]);
                    nxh264::inter::luma(&p, x, y, w, h, mv, &mut a, 0, 16);
                    nxh264::inter::luma_portable(&p, x, y, w, h, mv, &mut b, 0, 16);
                    if a != b {
                        bad += 1;
                        if bad <= 5 {
                            println!(
                                "h264bench: MISMATCH {}x{} at ({}, {}) mv {:?}",
                                w, h, x, y, mv
                            );
                        }
                    }
                }
            }
        }
    }
    let edges = nxh264::deblock::edge_selftest();
    if edges != 0 {
        println!("h264bench: deblocking edges: {} cases differ", edges);
    }
    bad += edges;
    println!(
        "h264bench: selftest {}",
        if bad == 0 {
            "PASS".to_string()
        } else {
            format!("FAIL ({} cases)", bad)
        }
    );
    bad == 0
}

/// `h264bench frames FILE`: the CRC32 of every decoded frame (as `tools/mkfixtures.py` writes them).
fn frames(path: &str) {
    let data = std::fs::read(path).expect("file");
    let table: Vec<u32> = (0..256u32)
        .map(|i| {
            (0..8).fold(i, |c, _| {
                if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                }
            })
        })
        .collect();
    let mut dec = nxh264::Decoder::new();
    dec.threads = 3;
    for au in access_units(&data) {
        if let Ok(Some(p)) = dec.decode(&au) {
            let crc = !p.to_i420().iter().fold(!0u32, |c, &b| {
                table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8)
            });
            println!("h264bench: crc {:08x}", crc);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("frames") {
        frames(&args[2]);
        return;
    }
    if args.get(1).map(String::as_str) == Some("selftest") {
        selftest();
        return;
    }
    if args.get(1).map(String::as_str) == Some("mem") {
        mem_bench();
        return;
    }
    if args.get(1).map(String::as_str) == Some("mc") {
        mc_bench();
        return;
    }
    let Some(path) = args.get(1) else {
        println!("usage: h264bench FILE.h264 [LOOPS [THREADS]]");
        return;
    };
    let loops: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
    let threads: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(1);
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            println!("h264bench: {}: {}", path, e);
            return;
        }
    };
    let crc = !data.iter().fold(!0u32, |c, &b| {
        let mut c = c ^ b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        c
    });
    println!("h264bench: file crc32 {:08x}", crc);
    let aus = access_units(&data);
    println!(
        "h264bench: {} access units, {} KB",
        aus.len(),
        data.len() / 1024
    );
    for round in 0..loops {
        let mut dec = nxh264::Decoder::new();
        dec.threads = threads;
        let (mut frames, mut errors) = (0u32, 0u32);
        let t = Instant::now();
        for (i, au) in aus.iter().enumerate() {
            match dec.decode(au) {
                Ok(Some(_)) => frames += 1,
                Ok(None) => {}
                Err(e) => {
                    errors += 1;
                    if errors <= 3 {
                        println!("h264bench: access unit {}: {:?}", i, e);
                    }
                }
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "h264bench: round {}: slices {:.1} ms, deblocking {:.1} ms a frame",
            round,
            dec.slice_ns as f64 / 1e6 / frames.max(1) as f64,
            dec.deblock_ns as f64 / 1e6 / frames.max(1) as f64
        );
        #[cfg(feature = "prof")]
        {
            use std::sync::atomic::Ordering::Relaxed;
            let f = frames.max(1) as f64 * 1e6;
            for (n, c) in nxh264::prof::NAMES.iter().zip(&nxh264::prof::NS) {
                println!(
                    "h264bench: {:>22}: {:.1} ms a frame (summed over threads)",
                    n,
                    c.swap(0, Relaxed) as f64 / f
                );
            }
        }
        println!(
            "h264bench: round {}: {} frames, {} errors, {:.1} ms a frame, {:.1} frames/s",
            round,
            frames,
            errors,
            ms / frames.max(1) as f64,
            frames as f64 * 1000.0 / ms
        );
    }
}
