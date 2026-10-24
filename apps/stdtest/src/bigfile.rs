//! Large-file integrity: a pattern file whose every 512-byte sector is
//! unique, written then read back, and a damaged read described (which
//! sectors, what they hold instead).
//!
//! `stdtest bigfile write FILE MB`, `bigfile verify FILE [PASSES [CHUNK_KB]]`,
//! `bigfile cmp FILE [PASSES [CHUNK_KB]]` (passes against the first, for a
//! file with no known content).

use std::collections::HashMap;
use std::fs;
use std::io::Read;

const SECTOR: usize = 512;

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Sector `n` of the pattern: 64 words from one seed per sector (cheap
/// enough for an emulated core to write and check 17 MiB).
pub fn sector(n: u64) -> [u8; SECTOR] {
    let seed = mix(n);
    let mut s = [0u8; SECTOR];
    for (w, b) in s.chunks_mut(8).enumerate() {
        b.copy_from_slice(&seed.wrapping_mul(2 * w as u64 + 1).rotate_left(w as u32).to_le_bytes());
    }
    s
}

pub fn pattern(sectors: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(sectors as usize * SECTOR);
    for n in 0..sectors {
        v.extend_from_slice(&sector(n));
    }
    v
}

fn hash(b: &[u8]) -> u64 {
    b.chunks(8).fold(0u64, |h, w| (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap_or([0; 8]))).wrapping_mul(0x517C_C1B7_2722_0A95))
}

/// What a damaged sector holds, in words.
fn describe(got: &[u8], want: &[u8], by_hash: &HashMap<u64, u64>) -> String {
    let bad = got.iter().zip(want).filter(|(a, b)| a != b).count();
    let first = got.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
    let kind = if got.iter().all(|&b| b == 0) {
        "zeros".to_string()
    } else if let Some(&other) = by_hash.get(&hash(got)) {
        format!("a copy of sector {other}")
    } else {
        "other bytes".to_string()
    };
    format!("{bad} bytes differ from byte {first}: {kind}")
}

/// Sector-by-sector report of `got` against `want`; the number of bad sectors.
fn report(label: &str, got: &[u8], want: &[u8], by_hash: &mut HashMap<u64, u64>) -> usize {
    if got.len() != want.len() {
        println!("{label}: {} bytes, expected {}", got.len(), want.len());
    }
    let mut bad = 0;
    let mut shown = 0;
    for (k, (g, w)) in got.chunks(SECTOR).zip(want.chunks(SECTOR)).enumerate() {
        if g == w {
            continue;
        }
        bad += 1;
        if shown < 40 {
            shown += 1;
            if by_hash.is_empty() {
                for (i, s) in want.chunks(SECTOR).enumerate() {
                    by_hash.insert(hash(s), i as u64);
                }
            }
            println!(
                "{label}: sector {k} (byte {:#x}, cluster-relative {}): {}",
                k * SECTOR,
                k % 64,
                describe(g, w, by_hash)
            );
        }
    }
    bad
}

fn read_all(path: &str, chunk: usize) -> std::io::Result<Vec<u8>> {
    if chunk == 0 {
        return fs::read(path);
    }
    let mut f = fs::File::open(path)?;
    let mut v = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(v);
        }
        v.extend_from_slice(&buf[..n]);
    }
}

fn arg<T: std::str::FromStr>(a: &[String], i: usize, d: T) -> T {
    a.get(i).and_then(|v| v.parse().ok()).unwrap_or(d)
}

pub fn run(a: &[String]) -> i32 {
    let (Some(mode), Some(path)) = (a.first(), a.get(1)) else {
        println!("usage: bigfile write FILE MB | verify FILE [PASSES [CHUNK_KB]] | cmp FILE [PASSES [CHUNK_KB]]");
        return 2;
    };
    match mode.as_str() {
        "write" => {
            let sectors = arg::<u64>(a, 2, 17) << 11;
            let data = pattern(sectors);
            match fs::write(path, &data) {
                Ok(()) => println!("RESULT bigfile wrote {} bytes", data.len()),
                Err(e) => {
                    println!("write: {e}");
                    return 1;
                }
            }
            0
        }
        "verify" | "cmp" => {
            let passes = arg::<usize>(a, 2, 4);
            let chunk = arg::<usize>(a, 3, 0) << 10;
            let mut base: Option<Vec<u8>> = None;
            let mut by_hash = HashMap::new();
            let mut bad_total = 0;
            for p in 0..passes {
                let got = match read_all(path, chunk) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("read {p}: {e}");
                        return 1;
                    }
                };
                let want = match (mode.as_str(), &base) {
                    ("verify", _) => pattern((got.len() / SECTOR) as u64 + (got.len() % SECTOR != 0) as u64),
                    (_, Some(b)) => b.clone(),
                    _ => Vec::new(),
                };
                let bad = if want.is_empty() {
                    0
                } else {
                    report(&format!("pass {p}"), &got, &want[..got.len().min(want.len())], &mut by_hash)
                };
                println!("pass {p}: {} bytes, {bad} bad sectors", got.len());
                bad_total += bad;
                if base.is_none() {
                    base = Some(got);
                }
            }
            println!("RESULT bigfile {bad_total} bad sectors in {passes} passes");
            (bad_total != 0) as i32
        }
        _ => 2,
    }
}

/// Suite test: a pattern file of `mb` MiB, bigger than the volume cache,
/// written then read back `passes` times. Slot reuse in the cache is
/// covered deterministically by `user/fs/src/clock.rs`'s tests; this one
/// checks the whole path (the 17 MiB case: `stdtest bigfile`).
pub fn selftest(mb: u64, passes: usize) -> Result<String, String> {
    let path = "/tmp/bigfile.bin";
    let data = pattern(mb << 11);
    fs::write(path, &data).map_err(|e| format!("write: {e}"))?;
    let mut by_hash = HashMap::new();
    let mut bad = 0;
    for p in 0..passes {
        let got = fs::read(path).map_err(|e| format!("read {p}: {e}"))?;
        bad += report(&format!("pass {p}"), &got, &data, &mut by_hash);
    }
    let _ = fs::remove_file(path);
    if bad != 0 {
        return Err(format!("{bad} damaged sectors in {passes} reads of {mb} MiB"));
    }
    Ok(format!("{mb} MiB read {passes} times"))
}
