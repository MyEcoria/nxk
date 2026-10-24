//! The decoder against ffmpeg's: each fixture is a short x264 clip of the
//! kind Luna's server sends, and `.crc` holds the CRC32 of every frame
//! ffmpeg decodes from it (`tools/mkfixtures.py`).

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    !data.iter().fold(!0u32, |c, &b| {
        table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8)
    })
}

/// Access units of an Annex B file: a new one starts at a slice with
/// first_mb_in_slice 0, parameter sets and SEI before it going along.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let (mut aus, mut cur, mut pending, mut has_slice) =
        (Vec::new(), Vec::new(), Vec::new(), false);
    for nal in nxh264::nal_units(data) {
        let mut bytes = vec![0, 0, 0, 1];
        bytes.extend_from_slice(nal);
        if matches!(nal[0] & 31, 1 | 5) {
            if nal[1] & 0x80 != 0 && has_slice {
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

fn check(name: &str) {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(format!("{}/{}.h264", dir, name)).unwrap();
    let crcs = std::fs::read_to_string(format!("{}/{}.crc", dir, name)).unwrap();
    let mut lines = crcs.lines();
    let size: Vec<usize> = lines
        .next()
        .unwrap()
        .split(' ')
        .map(|v| v.parse().unwrap())
        .collect();
    let want: Vec<u32> = lines.map(|l| u32::from_str_radix(l, 16).unwrap()).collect();
    let mut dec = nxh264::Decoder::new();
    let mut got = Vec::new();
    for au in access_units(&data) {
        if let Some(p) = dec.decode(&au).unwrap() {
            assert_eq!((p.width(), p.height()), (size[0], size[1]));
            got.push(crc32(&p.to_i420()));
        }
    }
    assert_eq!(got.len(), want.len(), "{}: frames", name);
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "{}: frame {}", name, i);
    }
}

#[test]
fn intra_slices() {
    check("intra_2slices");
}

#[test]
fn p_frames_one_slice() {
    check("p_1slice");
}

#[test]
fn p_frames_four_slices() {
    check("p_4slices");
}

#[test]
fn a_picture_that_is_not_a_multiple_of_sixteen() {
    check("p_odd_size");
}

#[test]
fn a_lost_reference_is_reported_and_the_next_key_frame_recovers() {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(format!("{}/p_1slice.h264", dir)).unwrap();
    let aus = access_units(&data);
    let mut dec = nxh264::Decoder::new();
    dec.decode(&aus[0]).unwrap();
    dec.decode(&aus[1]).unwrap();
    // Frame 2 is lost: frame 3 names a reference that is not there.
    assert!(matches!(dec.decode(&aus[3]), Err(nxh264::Error::Gap)));
    // The next IDR (keyint 8) starts over.
    assert!(dec.decode(&aus[8]).unwrap().is_some());
    assert!(dec.decode(&aus[9]).unwrap().is_some());
}

#[test]
fn garbage_is_refused_without_a_panic() {
    let mut dec = nxh264::Decoder::new();
    let mut x = 0x1234_5678u32;
    for len in [0usize, 3, 50, 500] {
        let mut junk: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let _ = dec.decode(&junk);
        junk.splice(0..0, [0, 0, 0, 1, 0x65]);
        let _ = dec.decode(&junk);
    }
}
