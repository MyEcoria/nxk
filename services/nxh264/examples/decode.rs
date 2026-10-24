//! `decode IN.h264 OUT.yuv [--no-deblock]`: decodes an Annex B file to I420
//! (to compare with ffmpeg's `-f rawvideo -pix_fmt yuv420p`).

use std::io::Write;

fn first_mb_is_zero(nal: &[u8]) -> bool {
    nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("input");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("output"));
    let mut dec = nxh264::Decoder::new();
    dec.deblock = !args.iter().any(|a| a == "--no-deblock");
    dec.threads = args
        .iter()
        .position(|a| a == "--threads")
        .and_then(|i| args.get(i + 1)?.parse().ok())
        .unwrap_or(1);
    let (mut aus, mut cur, mut pending): (Vec<Vec<u8>>, Vec<u8>, Vec<u8>) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut has_slice = false;
    for nal in nxh264::nal_units(&data) {
        let ty = nal[0] & 31;
        let mut bytes = vec![0, 0, 0, 1];
        bytes.extend_from_slice(nal);
        if ty == 1 || ty == 5 {
            if first_mb_is_zero(nal) && has_slice {
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
    let t = std::time::Instant::now();
    let (mut frames, mut errors) = (0, 0);
    for (i, au) in aus.iter().enumerate() {
        match dec.decode(au) {
            Ok(Some(p)) => {
                frames += 1;
                out.write_all(&p.to_i420()).unwrap();
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                eprintln!("access unit {}: {:?}", i, e);
            }
        }
    }
    eprintln!(
        "{} frames, {} errors, {:.1} ms a frame",
        frames,
        errors,
        t.elapsed().as_secs_f64() * 1000.0 / frames.max(1) as f64
    );
}
