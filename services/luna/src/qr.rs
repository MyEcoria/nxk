//! The sign-in page as a QR code, for `display` to show: black modules on
//! white, the 4-module quiet zone in, whole pixels per module.

use nxsvc::Cover;
use qrcode::{Color, EcLevel, QrCode};

/// `text` as a square image at most 256 pixels wide; `None` if too long.
pub fn image(text: &str) -> Option<Cover> {
    let code = QrCode::with_error_correction_level(text, EcLevel::M).ok()?;
    let n = code.width();
    let colors = code.to_colors();
    let total = n + 8;
    let k = (256 / total).max(1);
    Some(Cover::from_fn(total * k, |x, y| {
        let (mx, my) = ((x / k).wrapping_sub(4), (y / k).wrapping_sub(4));
        let dark = mx < n && my < n && colors[my * n + mx] == Color::Dark;
        if dark {
            0x000000
        } else {
            0xFFFFFF
        }
    }))
}
