//! Any bytes must decode or be refused, never panic (luna runs with
//! `panic = "abort"`): random packets for every CELT configuration and
//! frame-count code, and the vectors' packets with bits flipped and lost.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

fn vector_packets() -> Vec<Vec<u8>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let data = std::fs::read(e.unwrap().path()).unwrap();
        let mut at = 0;
        while at < data.len() {
            let len = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
            if len > 0 {
                out.push(data[at + 2..at + 2 + len].to_vec());
            }
            at += 2 + len + 4;
            let n = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
            at += 2 + n * 4;
        }
    }
    out
}

/// Rounds per test (`NXOPUS_FUZZ_ROUNDS` for a longer run).
fn rounds() -> usize {
    std::env::var("NXOPUS_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000)
}

#[test]
fn random_packets() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut dec = nxopus::Decoder::new();
    let mut pcm = vec![0i16; 2 * nxopus::MAX_FRAME];
    let (mut ok, mut refused) = (0, 0);
    for _ in 0..rounds() {
        let max = if rng.below(4) == 0 { 1500 } else { 300 };
        let len = 1 + rng.below(max) as usize;
        let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        p[0] = (16 + rng.below(16) as u8) << 3 | (rng.below(2) as u8) << 2 | rng.below(4) as u8;
        match dec.decode(&p, &mut pcm) {
            Ok(_) => ok += 1,
            Err(_) => refused += 1,
        }
    }
    eprintln!("random: {} decoded, {} refused", ok, refused);
    assert!(ok > rounds() / 4);
}

#[test]
fn flipped_packets() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let packets = vector_packets();
    let mut dec = nxopus::Decoder::new();
    let mut pcm = vec![0i16; 2 * nxopus::MAX_FRAME];
    for round in 0..rounds() {
        let mut p = packets[round % packets.len()].clone();
        for _ in 0..1 + rng.below(8) {
            let at = 1 + rng.below(p.len().max(2) as u32 - 1) as usize;
            if at < p.len() {
                p[at] ^= 1 << rng.below(8);
            }
        }
        if rng.below(8) == 0 {
            p.truncate(1 + rng.below(p.len() as u32) as usize);
        }
        if rng.below(6) == 0 {
            dec.conceal(&mut pcm);
        } else {
            let _ = dec.decode(&p, &mut pcm);
        }
    }
}

#[test]
fn silk_refused() {
    let mut dec = nxopus::Decoder::new();
    let mut pcm = vec![0i16; 2 * nxopus::MAX_FRAME];
    assert_eq!(
        dec.decode(&[0x08, 1, 2, 3], &mut pcm),
        Err(nxopus::Error::Unsupported)
    );
    assert_eq!(dec.decode(&[], &mut pcm), Err(nxopus::Error::InvalidPacket));
}
