//! The settings the screen remembers, in `/config/display.cfg` on the SD
//! card (`key=value` lines): theme, brightness, flight mode, auto-sleep.

use alloc::format;
use alloc::string::String;

use libnx::abi::proto::audio::DEFAULT_VOL;
use libnx::fs;

const DIR: &str = "/config";
const FILE: &str = "/config/display.cfg";
/// Auto-sleep choices, minutes (0: never).
pub const SLEEP_MIN: [u8; 5] = [0, 1, 5, 10, 30];

#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    pub light: bool,
    /// Backlight, % (`backlight::MIN_PCT`..=100); 0: leave what the loader set.
    pub brightness: u8,
    /// Speaker volume, % (0..=100).
    pub volume: u8,
    /// The Wi-Fi radio is off.
    pub flight: bool,
    /// Index in `SLEEP_MIN`.
    pub sleep: u8,
}

impl Config {
    pub const fn new() -> Self {
        Self {
            light: false,
            brightness: 0,
            volume: DEFAULT_VOL,
            flight: false,
            sleep: 0,
        }
    }

    /// The file, or `None` while the card is not readable.
    pub fn load() -> Option<Self> {
        let text = match fs::read(FILE) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            // No file yet counts only once the card is mounted.
            Err(e) if e == -libnx::abi::err::ENOENT && fs::statfs().is_ok() => String::new(),
            Err(_) => return None,
        };
        let mut c = Self::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "theme" => c.light = v == "light",
                "brightness" => c.brightness = v.parse().unwrap_or(0).min(100),
                "volume" => c.volume = v.parse().unwrap_or(DEFAULT_VOL).min(100),
                "flight" => c.flight = v == "on",
                "sleep" => c.sleep = v.parse().unwrap_or(0).min(SLEEP_MIN.len() as u8 - 1),
                _ => {}
            }
        }
        Some(c)
    }

    pub fn save(&self) -> Result<(), i64> {
        let _ = fs::mkdir(DIR);
        fs::write(
            FILE,
            format!(
                "theme={}\nbrightness={}\nvolume={}\nflight={}\nsleep={}\n",
                if self.light { "light" } else { "dark" },
                self.brightness,
                self.volume,
                if self.flight { "on" } else { "off" },
                self.sleep
            )
            .as_bytes(),
        )
    }
}
