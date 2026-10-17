//! The dark and light palettes, chosen at run time (`set`): the UI asks
//! `pal()` for every color it draws. Colors are A B G R, as `screen`'s.
//! Basic Black and Basic White, as the Switch 2's: the selection frame
//! and the selected item's name in its cyan.

use core::sync::atomic::{AtomicBool, Ordering};

pub const fn rgb(c: u32) -> u32 {
    0xFF00_0000 | (c & 0xFF) << 16 | (c >> 8 & 0xFF) << 8 | c >> 16
}

pub const WHITE: u32 = 0xFFFF_FFFF;
pub const BLACK: u32 = 0xFF00_0000;

pub struct Palette {
    /// Page background; the drawer, sheets and cards on it; a button.
    pub bg: u32,
    pub surface: u32,
    pub raised: u32,
    /// Rules: on the background, and on a surface.
    pub rule: u32,
    pub line: u32,
    /// Empty part of a bar or slider.
    pub track: u32,
    pub text: u32,
    pub dim: u32,
    /// Disabled controls.
    pub faint: u32,
    /// The selected item's name, links, the chosen option.
    pub accent: u32,
    /// Text on an accent-colored control.
    pub on_accent: u32,
    /// "On", sliders, things that work.
    pub good: u32,
    pub warn: u32,
    pub bad: u32,
    /// The frame around what the gamepad has selected.
    pub focus: u32,
    /// Darkens what is behind a sheet.
    pub scrim: u32,
}

pub const DARK: Palette = Palette {
    bg: rgb(0x000000),
    surface: rgb(0x1A1A1A),
    raised: rgb(0x262626),
    rule: rgb(0x3A3A3A),
    line: rgb(0x393939),
    track: rgb(0x5D5D5D),
    text: rgb(0xFFFFFF),
    dim: rgb(0xC4C4C4),
    faint: rgb(0x6A6A6A),
    accent: rgb(0x3BC3E3),
    on_accent: rgb(0x000000),
    good: rgb(0x6EEACD),
    warn: rgb(0xFFB84D),
    bad: rgb(0xFF5A50),
    focus: rgb(0x3BC3E3),
    scrim: rgb(0x000000),
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xEBEBEB),
    surface: rgb(0xF7F7F7),
    raised: rgb(0xFFFFFF),
    rule: rgb(0xD2D2D2),
    line: rgb(0xDADADA),
    track: rgb(0xBDBDBD),
    text: rgb(0x2D2D2D),
    dim: rgb(0x606060),
    faint: rgb(0xA8A8A8),
    accent: rgb(0x0096C8),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x00A88A),
    warn: rgb(0xC77700),
    bad: rgb(0xD93030),
    focus: rgb(0x0AB4DC),
    scrim: rgb(0x2D2D2D),
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
