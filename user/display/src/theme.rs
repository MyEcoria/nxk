//! The dark and light palettes, chosen at run time (`set`): the UI asks
//! `pal()` for every color it draws. Colors are A B G R, as `screen`'s.

use core::sync::atomic::{AtomicBool, Ordering};

pub const fn rgb(c: u32) -> u32 {
    0xFF00_0000 | (c & 0xFF) << 16 | (c >> 8 & 0xFF) << 8 | c >> 16
}

pub struct Palette {
    /// Page background, cards on it, a raised or pressed control, outlines.
    pub bg: u32,
    pub surface: u32,
    pub raised: u32,
    pub line: u32,
    /// Empty part of a bar or slider.
    pub track: u32,
    pub text: u32,
    pub dim: u32,
    pub accent: u32,
    /// Text on an accent-colored control.
    pub on_accent: u32,
    pub good: u32,
    pub warn: u32,
    pub bad: u32,
    /// Outline of what the gamepad has selected.
    pub focus: u32,
    /// Darkens what is behind a sheet.
    pub scrim: u32,
}

pub const DARK: Palette = Palette {
    bg: rgb(0x08090B),
    surface: rgb(0x141518),
    raised: rgb(0x222429),
    line: rgb(0x282A30),
    track: rgb(0x383B42),
    text: rgb(0xF5F6F8),
    dim: rgb(0x8A8E96),
    accent: rgb(0x20A4F3),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x2EE59D),
    warn: rgb(0xFFB84D),
    bad: rgb(0xFF5252),
    focus: rgb(0xDCE0E8),
    scrim: rgb(0x000000),
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xF3F4F7),
    surface: rgb(0xFFFFFF),
    raised: rgb(0xE8EAEF),
    line: rgb(0xD8DCE4),
    track: rgb(0xCFD4DE),
    text: rgb(0x14161A),
    dim: rgb(0x6E737D),
    accent: rgb(0x0084D6),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x0FA968),
    warn: rgb(0xC77700),
    bad: rgb(0xD93030),
    focus: rgb(0x14161A),
    scrim: rgb(0x14161A),
};

static LIGHT_ON: AtomicBool = AtomicBool::new(false);

pub fn set(light: bool) {
    LIGHT_ON.store(light, Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT_ON.load(Ordering::Relaxed)
}

pub fn pal() -> &'static Palette {
    if is_light() {
        &LIGHT
    } else {
        &DARK
    }
}
