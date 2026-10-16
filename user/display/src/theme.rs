//! The dark and light palettes, chosen at run time (`set`): the UI asks
//! `pal()` for every color it draws. Colors are A B G R, as `screen`'s.

use core::sync::atomic::{AtomicBool, Ordering};

const fn rgb(c: u32) -> u32 {
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
    bg: rgb(0x17181A),
    surface: rgb(0x232427),
    raised: rgb(0x2F3136),
    line: rgb(0x3C3E44),
    track: rgb(0x3C3E44),
    text: rgb(0xF2F2F2),
    dim: rgb(0x9A9CA3),
    accent: rgb(0x00BFE8),
    on_accent: rgb(0x06161B),
    good: rgb(0x3DDBB0),
    warn: rgb(0xFFB84D),
    bad: rgb(0xFF5C5C),
    focus: rgb(0xFFFFFF),
    scrim: rgb(0x000000),
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xEBEBED),
    surface: rgb(0xFFFFFF),
    raised: rgb(0xF1F2F4),
    line: rgb(0xD4D5D9),
    track: rgb(0xD9DADE),
    text: rgb(0x2B2C30),
    dim: rgb(0x71737B),
    accent: rgb(0x0096C2),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x0E9C76),
    warn: rgb(0xC77700),
    bad: rgb(0xD23232),
    focus: rgb(0x00A9D6),
    scrim: rgb(0x20222A),
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
