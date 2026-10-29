//! The boot screen as a raw image (little-endian 32-bit pixels), for the
//! screen tests: `render WIDTH HEIGHT rgba|bgra PROGRESS OUT`. A portrait
//! size (HEIGHT > WIDTH) is drawn turned, as on the Switch's panel.

use nxk_bootsplash::{turn, Format, Splash};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 6 {
        eprintln!("usage: render WIDTH HEIGHT rgba|bgra PROGRESS OUT");
        std::process::exit(2);
    }
    let (iw, ih): (u32, u32) = (a[1].parse().unwrap(), a[2].parse().unwrap());
    let fmt = if a[3] == "bgra" {
        Format::Bgra
    } else {
        Format::Rgba
    };
    let p: u32 = a[4].parse().unwrap();
    let mut img = vec![0u32; (iw * ih) as usize];
    Splash::new(iw.max(ih), iw.min(ih)).draw(p, &mut |x, y, w, h, c| {
        let (x, y, w, h) = if ih > iw {
            turn(x, y, w, h, ih)
        } else {
            (x, y, w, h)
        };
        for r in y..y + h {
            for k in x..x + w {
                img[(r * iw + k) as usize] = fmt.pixel(c);
            }
        }
    });
    let bytes: Vec<u8> = img.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(&a[5], bytes).unwrap();
}
