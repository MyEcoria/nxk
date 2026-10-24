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

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
        for au in &aus {
            match dec.decode(au) {
                Ok(Some(_)) => frames += 1,
                Ok(None) => {}
                Err(_) => errors += 1,
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "h264bench: round {}: slices {:.1} ms, deblocking {:.1} ms a frame",
            round,
            dec.slice_ns as f64 / 1e6 / frames.max(1) as f64,
            dec.deblock_ns as f64 / 1e6 / frames.max(1) as f64
        );
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
