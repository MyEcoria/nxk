//! nxopus against libopus 1.6.1 (tests/gen_vectors.py): every packet must
//! leave the same final range (the entropy decoding is bit-exact) and the
//! samples must match within float rounding. Lost packets (length 0) are
//! concealed: there the float kernels' summation order (libopus uses SIMD
//! ones on the PC) may move the pitch found, so they are held to less.

use std::path::Path;

struct Record<'a> {
    packet: &'a [u8],
    range: u32,
    pcm: Vec<i16>,
}

fn records(data: &[u8]) -> Vec<Record<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let len = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
        at += 2;
        let packet = &data[at..at + len];
        at += len;
        let range = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        let n = u16::from_le_bytes([data[at + 4], data[at + 5]]) as usize;
        at += 6;
        let pcm = data[at..at + n * 4]
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        at += n * 4;
        out.push(Record { packet, range, pcm });
    }
    out
}

/// SNR in dB of `got` against `want` (their squared sums).
fn snr(sig: f64, err: f64) -> f64 {
    if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig.max(1.0) / err).log10()
    }
}

/// Decodes a vector file; returns (packets, range mismatches, SNR of the
/// decoded packets, SNR of the concealed ones) in dB.
fn check(path: &Path) -> (usize, usize, f64, f64) {
    let data = std::fs::read(path).unwrap();
    let mut dec = nxopus::Decoder::new();
    let (mut bad, mut sig, mut err) = (0, 0f64, 0f64);
    let (mut lsig, mut lerr) = (0f64, 0f64);
    let recs = records(&data);
    let mut pcm = vec![0i16; 2 * nxopus::MAX_FRAME];
    for (k, r) in recs.iter().enumerate() {
        let lost = r.packet.is_empty();
        let n = if lost {
            dec.conceal(&mut pcm[..r.pcm.len()])
        } else {
            dec.decode(r.packet, &mut pcm).unwrap()
        };
        assert_eq!(2 * n, r.pcm.len());
        if dec.final_range() != r.range {
            if bad == 0 {
                eprintln!(
                    "{}: packet {} range {:08x}, libopus {:08x}",
                    path.display(),
                    k,
                    dec.final_range(),
                    r.range
                );
            }
            bad += 1;
        }
        let (s, e) = if lost {
            (&mut lsig, &mut lerr)
        } else {
            (&mut sig, &mut err)
        };
        for (a, b) in pcm[..2 * n].iter().zip(&r.pcm) {
            *s += (*b as f64).powi(2);
            *e += (*a as f64 - *b as f64).powi(2);
        }
    }
    (recs.len(), bad, snr(sig, err), snr(lsig, lerr))
}

#[test]
fn libopus_vectors() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut failed = false;
    for path in names {
        let (n, bad, snr, lost) = check(&path);
        eprintln!(
            "{}: {} packets, {} range mismatches, SNR {:.1} dB, concealed {:.1} dB",
            path.file_name().unwrap().to_string_lossy(),
            n,
            bad,
            snr,
            lost
        );
        failed |= bad != 0 || snr < 60.0 || lost < 50.0;
    }
    assert!(!failed);
}

/// A capture of the live stream with its libopus decoding (never committed):
/// `NXOPUS_CAPTURE=path cargo test -- --ignored`.
#[test]
#[ignore]
fn capture() {
    let path = std::env::var("NXOPUS_CAPTURE").expect("NXOPUS_CAPTURE");
    let (n, bad, snr, _) = check(Path::new(&path));
    eprintln!(
        "capture: {} packets, {} range mismatches, SNR {:.1} dB",
        n, bad, snr
    );
    assert!(bad == 0 && snr >= 60.0);
}
