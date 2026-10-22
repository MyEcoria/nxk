//! The Switch 2 touch and gamepad UI: the frame every page shares
//! (player avatar on the home screen, icon + title header on inner pages,
//! status cluster, bottom hint bar), the selection the gamepad moves, and
//! what a tap or a button does. The pages draw themselves: `home`, `quick`
//! (the drawer), `settings`, `sheets` (sheets and confirmations) and
//! `pages` (System, Network, Terminal, Logs).

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{cpufreq, fan, gpu, input, power};

use crate::art;
use crate::backlight::MIN_PCT;
use crate::catalog::{ServiceInfo, View};
use crate::config::{Config, SLEEP_MIN};
use crate::flow::Flow;
use crate::font::{self, Face};
use crate::games::{Cover, GameInfo};
use crate::playtime::Playtime;
use crate::screen::{mix, Screen};
use crate::stats::{Cpu, Data, Fan, Gpu, Seen};
use crate::status::{clock, gpu_mhz, wifi_connected, wifi_short};
use crate::term::Term;
use crate::theme::{self, pal};
use crate::users::Users;
use crate::widgets::{battery, button_glyph, icon, press, tone_color, volume_hud, Icon, Tone};
use crate::Messages;

/// The console's own games (`/games`), the first catalog.
pub const INTERN: &str = "Intern";
/// Covers kept around the selected game of a catalog, and read ahead.
const COVERS_KEPT: usize = 16;
const COVERS_AHEAD: usize = 6;

/// The layout is made for the Switch's 1280x720 (landscape).
pub const W: usize = 1280;
pub const H: usize = 720;
/// Side margin, content top and bottom (between the header rule and the footer rule).
pub const M: usize = 48;
pub const CY: usize = 132;
pub const CB: usize = 616;
pub const CH: usize = CB - CY;
/// Center lines of the top bar and the hint bar; the header and footer rules.
pub const STATUS_Y: usize = 54;
pub const HEADER_RULE_Y: usize = 104;
pub const RULE_Y: usize = 640;
pub const HINT_Y: usize = 679;

// Pages.
pub const LIBRARY: usize = 0;
pub const SYSTEM: usize = 1;
pub const NETWORK: usize = 2;
pub const TERMINAL: usize = 3;
pub const LOGS: usize = 4;
pub const SETTINGS: usize = 5;
pub const CONTROLLERS: usize = 6;
pub const USERS: usize = 7;

const TOAST_NS: u64 = 4_000_000_000;
const FAN_STEP: u64 = 10;
const PAD_STEP: u8 = 5;
const VOL_HUD_NS: u64 = 1_800_000_000;

#[derive(Clone, Copy, PartialEq)]
pub enum Id {
    AvatarHome,
    /// A game's square icon, by index.
    Game(usize),
    /// A round button under the game row (`home::BUTTONS`).
    Tile(usize),
    /// The status at the top right (opens the drawer).
    StatusQuick,
    /// Closes what is on top (the dimmed area).
    Scrim,
    /// The volume indicator, and the level bar in it (a slider).
    Hud,
    HudVolume,
    // The drawer.
    QSleep,
    QBright,
    QVolume,
    QFlight,
    QSettings,
    // Settings.
    Cat(usize),
    Theme(bool),
    Bright,
    Volume,
    Mute,
    PlayMusic,
    PlayTone,
    AutoSleep(usize),
    Flight,
    OpenPage(usize),
    SleepNow,
    Reboot,
    PowerOff,
    // System.
    SysTab(usize),
    /// A tab of the Controllers page.
    CtlTab(usize),
    // Network.
    Scan,
    Retry,
    Leave,
    ScanBack,
    // Controllers.
    BtPair,
    /// A paired controller, by its index in `bt_pads`.
    BtForget(usize),
    LogSize,
    LogClear,
    TermSize,
    // Sheets.
    FanOpen,
    FanAuto,
    FanManual,
    FanDown,
    FanUp,
    CpuOpen,
    /// A step of `cpufreq::STEPS_MHZ`, by index.
    CpuStep(usize),
    CpuBoot,
    GpuOpen,
    /// A step of `gpu::STEPS_KHZ`, by index.
    GpuStep(usize),
    SheetClose,
    No,
    Yes,
    // Users page.
    UserRow(usize),
    UserAdd,
    UserSwitch,
    UserUnlock,
    UserRename,
    UserLook,
    UserPin,
    UserDelete,
    /// Erase the saves of a game, by index in `Ui::saves`.
    SaveErase(usize),
    SavesPrev,
    SavesNext,
    // User flows: the keyboard (a lowercase letter or digit), the avatar
    // choices, the PIN pad and the buttons under every step.
    Key(u8),
    KeySpace,
    KeyBack,
    KeyShift,
    Glyph(usize),
    Color(usize),
    Pin(usize),
    PinBack,
    FlowNext,
    FlowBack,
    FlowSkip,
    LockOther,
    /// A catalog's tab on the home screen, by index in `Ui::services`.
    Service(usize),
    /// Cancel the catalog game being started.
    Cancel,
}

/// What a tap asks the service to do.
#[derive(Clone, Copy)]
pub enum Action {
    Power(u32),
    Scan,
    Retry,
    Leave,
    ClearLogs,
    /// Start game `n` of the library.
    Launch(usize),
    /// `fan::SET`: `fan::AUTO` or a duty (%).
    Fan(u64),
    /// `cpufreq::SET`: a step (MHz) or `cpufreq::BOOT`.
    Cpu(u64),
    /// `gpu::CLOCK`: a step (kHz), with `CLOCK_FORCE` past the cap.
    Gpu(u64, bool),
    /// Backlight, %.
    Brightness(u8),
    /// Speaker volume, % (0..=100).
    Volume(u8),
    /// Play the built-in polyphonic test melody.
    Music,
    /// Play a 440 Hz reference sine tone.
    Tone,
    /// A setting changed: write the file soon.
    Save,
    /// Screen off.
    Sleep,
    /// Flight mode on or off (`wifi::RADIO`).
    Flight(bool),
    /// Bluetooth pairing window opened (true) or closed (`bt::PAIR`).
    BtPair(bool),
    /// Forget the controller at this address (`bt::FORGET`).
    BtForget(u64),
    /// Write user `n`'s profile; the flag: it also starts playing.
    UserSave(usize, bool),
    /// User `n` plays now.
    UserActivate(usize),
    /// Erase user `n` from the card.
    UserDelete(usize),
    /// Erase user `n`'s saves of the game at this index of `Ui::saves`.
    EraseSave(usize, usize),
    /// Show the games of catalog `n` of `Ui::services`.
    Service(usize),
    /// Stop the catalog game being started.
    Cancel,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Sheet {
    Fan,
    Cpu,
    Gpu,
    Power,
}

/// A place a tap or the gamepad's A acts on.
pub(crate) struct Zone {
    pub id: Id,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub r: usize,
}

pub(crate) struct Modal {
    pub title: String,
    pub lines: [String; 2],
    pub ok: &'static str,
    pub tone: Tone,
    pub action: Action,
}

pub struct ScanNet {
    pub ssid: String,
    pub channel: u8,
    pub signal: u8,
    pub security: u8,
}

pub struct Ui {
    pub page: usize,
    pub(crate) pressed: Option<Id>,
    pub(crate) modal: Option<Modal>,
    toast: Option<(String, u64)>,
    /// When the volume indicator hides (0: not shown).
    vol_hud: u64,
    pub scan: Option<Vec<ScanNet>>,
    pub busy: bool,
    pub(crate) log_scale: usize,
    pub(crate) term_scale: usize,
    pub unread: usize,
    pub(crate) ssid: String,
    /// The Controllers page as last drawn: pairing under way, and the
    /// paired controllers' addresses and names.
    pub(crate) bt_pairing: bool,
    pub(crate) bt_pads: Vec<(u64, String)>,
    pub(crate) sheet: Option<Sheet>,
    /// The drawer on the right (`+`, `-` or the status).
    pub quick: bool,
    /// A game holds the screen: only the overlay is drawn (`draw_over`),
    /// the drawer without the entries that leave the game.
    pub in_game: bool,
    pub(crate) fan: Seen<Fan>,
    fan_sent: usize,
    fan_last: u64,
    pub(crate) cpu: Seen<Cpu>,
    pub(crate) cpu_sent: bool,
    pub(crate) gpu: Seen<Gpu>,
    pub(crate) gpu_sent: bool,
    pub(crate) zones: Vec<Zone>,
    /// The user flow on screen (setup, new user, PIN...), and whether its
    /// step is sliding in this frame.
    pub(crate) flow: Option<Flow>,
    pub(crate) sliding: bool,
    pub users: Users,
    /// Users page: the profile shown, what each game keeps for it, and
    /// the page of that list.
    pub(crate) usel: usize,
    pub(crate) saves: Vec<(String, u64)>,
    pub(crate) saves_req: bool,
    pub(crate) saves_page: usize,
    /// What the gamepad has selected outside the home row.
    pub(crate) focus: Option<Id>,
    focus_at: (usize, usize),
    pub(crate) games: Vec<GameInfo>,
    /// The home selection: a game (`0..games.len()`), then the round buttons.
    pub(crate) home: usize,
    /// Last selected game slot on the home row.
    pub(crate) last_game: usize,
    pub(crate) scroll: f32,
    pub(crate) scroll_to: f32,
    pub cfg: Config,
    pub play: Playtime,
    pub has_backlight: bool,
    pub(crate) cat: usize,
    pub(crate) sys_tab: usize,
    /// Controllers page: its tab, each controller's live state, the left
    /// stick's direction held and when it repeats.
    pub(crate) ctl_tab: usize,
    pub(crate) pads: Vec<crate::controllers::PadView>,
    pub(crate) nav: Option<u64>,
    pub(crate) nav_at: u64,
    drag: Option<(Id, usize, usize)>,
    /// Wall clock of the frame being drawn.
    pub(crate) wall: u64,
    /// The game catalogs ("services"): name, title; the console's own
    /// games first, with no name.
    pub(crate) services: Vec<(String, String)>,
    /// The catalog shown, by name (empty: the console's own games).
    pub service: String,
    /// The running catalog's state; `None` with the console's games.
    pub prov: Option<View>,
    /// The catalog game being started: its title, cover, when.
    pub starting: Option<(String, Option<Cover>, u64)>,
    /// The catalog list `games` holds, and the covers asked of it.
    pub prov_list: u64,
    pub(crate) covers_asked: Vec<String>,
    /// Covers the catalog had not (`ENOENT`): asked again when its covers
    /// change.
    pub covers_absent: Vec<String>,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            page: LIBRARY,
            pressed: None,
            modal: None,
            toast: None,
            vol_hud: 0,
            scan: None,
            busy: false,
            log_scale: 1,
            term_scale: 1,
            unread: 0,
            ssid: String::new(),
            bt_pairing: false,
            bt_pads: Vec::new(),
            sheet: None,
            quick: false,
            in_game: false,
            fan: Seen::Unknown,
            fan_sent: 0,
            fan_last: fan::AUTO,
            cpu: Seen::Unknown,
            cpu_sent: false,
            gpu: Seen::Unknown,
            gpu_sent: false,
            zones: Vec::new(),
            flow: None,
            sliding: false,
            users: Users::default(),
            usel: 0,
            saves: Vec::new(),
            saves_req: false,
            saves_page: 0,
            focus: None,
            focus_at: (W / 2, H / 2),
            games: Vec::new(),
            home: 0,
            last_game: 0,
            scroll: 0.0,
            scroll_to: 0.0,
            cfg: Config::new(),
            play: Playtime::default(),
            has_backlight: false,
            cat: 0,
            sys_tab: 0,
            ctl_tab: 0,
            pads: Vec::new(),
            nav: None,
            nav_at: 0,
            drag: None,
            wall: 0,
            services: alloc::vec![(String::new(), INTERN.into())],
            service: String::new(),
            prov: None,
            starting: None,
            prov_list: 0,
            covers_asked: Vec::new(),
            covers_absent: Vec::new(),
        }
    }

    /// The settings came from the card (or changed): theme in force.
    pub fn apply_config(&mut self, c: Config) {
        self.cfg = c;
        theme::set(c.light);
    }

    /// The playing user's theme is in force.
    pub fn apply_theme(&mut self, light: bool) {
        self.cfg.light = light;
        theme::set(light);
    }

    /// A fan request came back.
    pub fn fan_done(&mut self) {
        self.fan_sent = self.fan_sent.saturating_sub(1);
    }

    pub(crate) fn cpu_step(&self) -> Option<u64> {
        match self.cpu {
            Seen::Value(c) => Some(c.step),
            _ => None,
        }
    }

    /// A clock change came back.
    pub fn cpu_done(&mut self) {
        self.cpu_sent = false;
    }

    pub fn gpu_done(&mut self) {
        self.gpu_sent = false;
    }

    fn gpu_step(&mut self, i: usize) -> Option<Action> {
        let Seen::Value(g) = self.gpu else {
            return None;
        };
        let k = gpu::STEPS_KHZ[i];
        if g.step == k {
            return None;
        }
        if k <= g.cap {
            self.gpu_sent = true;
            return Some(Action::Gpu(k, false));
        }
        let lines = if g.supply == power::SUPPLY_EXTERNAL {
            [
                alloc::format!("Above the {} MHz cap on input power:", gpu_mhz(g.cap)),
                "the official charger or dock only.".into(),
            ]
        } else {
            [
                alloc::format!("Above the {} MHz cap on battery, which", gpu_mhz(g.cap)),
                "sys-clk keeps to spare the battery.".into(),
            ]
        };
        self.modal = Some(Modal {
            title: alloc::format!("Overclock the GPU to {} MHz?", gpu_mhz(k)),
            lines,
            ok: "Overclock",
            tone: Tone::Warn,
            action: Action::Gpu(k, true),
        });
        None
    }

    pub(crate) fn fan_manual(&self) -> Option<u64> {
        if self.fan_sent > 0 {
            (self.fan_last != fan::AUTO).then_some(self.fan_last)
        } else {
            match self.fan {
                Seen::Value(f) => f.manual,
                _ => None,
            }
        }
    }

    fn fan_set(&mut self, v: u64) -> Option<Action> {
        self.fan_sent += 1;
        self.fan_last = v;
        Some(Action::Fan(v))
    }

    /// A new game list: the last played first, as the Switch, and the
    /// selection kept on the same game.
    pub fn set_games(&mut self, mut v: Vec<GameInfo>) {
        let kept = self.games.get(self.home).map(|g| g.name.clone());
        let order = |g: &GameInfo| self.play.get(&g.name).map_or(0, |e| e.order);
        // A catalog's own order stands after the last played (stable sort).
        v.sort_by(|a, b| {
            order(b).cmp(&order(a)).then_with(|| {
                if a.id.is_empty() {
                    a.title.cmp(&b.title)
                } else {
                    core::cmp::Ordering::Equal
                }
            })
        });
        let old = self.games.len();
        self.home = match kept {
            Some(name) => v.iter().position(|g| g.name == name).unwrap_or(0),
            None if old == 0 => 0,
            None => self.home - old + v.len(),
        };
        self.last_game = self.home.min(v.len().saturating_sub(1));
        self.games = v;
    }

    /// The games, for `games::scan` to keep their covers.
    pub fn take_games(&mut self) -> Vec<GameInfo> {
        core::mem::take(&mut self.games)
    }

    pub fn game(&self, i: usize) -> Option<&GameInfo> {
        self.games.get(i)
    }

    /// The catalogs on the card, after the console's own games.
    pub fn set_services(&mut self, list: Vec<ServiceInfo>) {
        self.services.truncate(1);
        self.services
            .extend(list.into_iter().map(|s| (s.name, s.title)));
        // The one picked keeps its tab while it is not on the card.
        if !self.service.is_empty() && !self.services.iter().any(|(n, _)| *n == self.service) {
            self.services
                .push((self.service.clone(), self.service.clone()));
        }
    }

    /// The catalog shown, by index in `services` (0 if it is gone).
    pub fn service_index(&self) -> usize {
        self.services
            .iter()
            .position(|(n, _)| *n == self.service)
            .unwrap_or(0)
    }

    /// A catalog game is starting or running: the catalog must stay.
    pub fn catalog_busy(&self) -> bool {
        self.starting.is_some()
            || self.prov.as_ref().is_some_and(|v| {
                matches!(
                    v.state,
                    libnx::abi::proto::provider::ST_LAUNCHING
                        | libnx::abi::proto::provider::ST_PLAYING
                )
            })
    }

    /// Show catalog `k`'s games: the list goes, the catalog's comes.
    pub fn pick_service(&mut self, k: usize) {
        if let Some((name, _)) = self.services.get(k) {
            self.show_service(name.clone());
        }
    }

    /// Show the games of the catalog named `name` (a user's choice; it
    /// may not be on the card yet).
    pub fn show_service(&mut self, name: String) {
        self.service = name;
        self.games.clear();
        self.prov = None;
        self.prov_list = 0;
        self.covers_asked.clear();
        self.covers_absent.clear();
        self.home = 0;
        self.last_game = 0;
        self.scroll = 0.0;
        self.scroll_to = 0.0;
    }

    /// L or R on the home screen: the catalog before or after.
    fn step_service(&mut self, next: bool) -> Option<Action> {
        let n = self.services.len();
        let k = self.service_index();
        Some(Action::Service(if next {
            (k + 1) % n
        } else {
            (k + n - 1) % n
        }))
    }

    /// A catalog's covers to read now (the games near the selection), the
    /// ones far from it dropped.
    pub fn want_covers(&mut self) -> Option<Vec<String>> {
        if self.service.is_empty() || self.page != LIBRARY || self.games.is_empty() {
            return None;
        }
        let at = self.last_game.min(self.games.len() - 1);
        let mut near = Vec::new();
        for (i, g) in self.games.iter_mut().enumerate() {
            if i.abs_diff(at) > COVERS_KEPT {
                g.cover = None;
            } else {
                near.push(g.id.clone());
            }
        }
        self.covers_asked.retain(|id| near.contains(id));
        let lo = at.saturating_sub(COVERS_AHEAD);
        let hi = (at + COVERS_AHEAD).min(self.games.len() - 1);
        let mut idx: Vec<usize> = (lo..=hi)
            .filter(|&i| {
                let g = &self.games[i];
                g.cover.is_none()
                    && !self.covers_asked.contains(&g.id)
                    && !self.covers_absent.contains(&g.id)
            })
            .collect();
        idx.sort_by_key(|&i| i.abs_diff(at));
        let want: Vec<String> = idx.iter().map(|&i| self.games[i].id.clone()).collect();
        self.covers_asked.extend(want.iter().cloned());
        (!want.is_empty()).then_some(want)
    }

    /// A cover not read: asked again soon, or once the catalog's covers
    /// change when it had none (`absent`).
    pub fn cover_missed(&mut self, list: u64, id: &str, absent: bool) {
        if list != self.prov_list {
            return;
        }
        self.covers_asked.retain(|a| a != id);
        if absent && !self.covers_absent.iter().any(|a| a == id) {
            self.covers_absent.push(id.into());
        }
    }

    /// A catalog's cover arrived (for list `list`).
    pub fn set_cover(&mut self, list: u64, id: &str, c: Cover) {
        if list != self.prov_list {
            return;
        }
        if let Some(g) = self.games.iter_mut().find(|g| g.id == id) {
            g.cover = Some(c);
        }
    }

    pub fn animating(&self) -> bool {
        self.flow.is_some() || (self.page == LIBRARY && (self.scroll - self.scroll_to).abs() > 0.5)
    }

    /// Time between two frames of an animation: the flows redraw often
    /// but change little, the home row's scroll moves all of it.
    pub fn frame_ns(&self) -> u64 {
        if self.flow.is_some() {
            33_000_000
        } else {
            16_000_000
        }
    }

    pub fn toast(&mut self, text: String, now: u64) {
        self.toast = Some((text, now + TOAST_NS));
    }

    pub fn deadline(&self) -> Option<u64> {
        let hud = (self.vol_hud != 0).then_some(self.vol_hud);
        match (self.toast.as_ref().map(|t| t.1), hud) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// A volume button: one step and the indicator (the level may already
    /// be at its end; the indicator still shows).
    pub fn volume_step(&mut self, up: bool, now: u64) -> Option<Action> {
        self.vol_hud = now + VOL_HUD_NS;
        let v = self.cfg.volume;
        self.set_volume(if up {
            v.saturating_add(PAD_STEP)
        } else {
            v.saturating_sub(PAD_STEP)
        })
    }

    pub(crate) fn zone(&mut self, id: Id, x: usize, y: usize, w: usize, h: usize, r: usize) {
        self.zones.push(Zone { id, x, y, w, h, r });
    }

    fn hit(&self, x: usize, y: usize) -> Option<Id> {
        self.zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x < z.x + z.w && y >= z.y && y < z.y + z.h)
            .map(|z| z.id)
    }

    /// What the gamepad's selection can land on.
    fn focusable(id: Id) -> bool {
        !matches!(id, Id::Scrim | Id::Hud | Id::HudVolume)
    }

    fn nearest(&self, cx: usize, cy: usize) -> Option<Id> {
        self.zones
            .iter()
            .filter(|z| Self::focusable(z.id))
            .min_by_key(|z| {
                let dx = (z.x + z.w / 2) as i64 - cx as i64;
                let dy = (z.y + z.h / 2) as i64 - cy as i64;
                dx * dx + dy * dy
            })
            .map(|z| z.id)
    }

    /// The zone nearest to the selection in the direction of `code`.
    pub(crate) fn step_focus(&mut self, code: u64) {
        let (dx, dy): (i64, i64) = match code {
            input::BTN_LEFT => (-1, 0),
            input::BTN_RIGHT => (1, 0),
            input::BTN_UP => (0, -1),
            input::BTN_DOWN => (0, 1),
            _ => return,
        };
        let cur = self
            .focus
            .and_then(|f| self.zones.iter().find(|z| z.id == f));
        let Some(cur) = cur else {
            self.focus = self
                .zones
                .iter()
                .find(|z| Self::focusable(z.id))
                .map(|z| z.id);
            return;
        };
        let (cx, cy) = ((cur.x + cur.w / 2) as i64, (cur.y + cur.h / 2) as i64);
        let next = self
            .zones
            .iter()
            .filter(|z| z.id != cur.id && Self::focusable(z.id))
            .filter_map(|z| {
                let (zx, zy) = ((z.x + z.w / 2) as i64 - cx, (z.y + z.h / 2) as i64 - cy);
                let along = zx * dx + zy * dy;
                let perp = (zx * dy - zy * dx).abs();
                (along > 0 && perp <= along * 2).then_some((along + perp * 3, z.id))
            })
            .min_by_key(|t| t.0)
            .map(|t| t.1);
        if let Some(id) = next {
            self.focus = Some(id);
        }
    }

    fn is_slider(id: Id) -> bool {
        matches!(
            id,
            Id::QBright | Id::Bright | Id::QVolume | Id::Volume | Id::HudVolume
        )
    }

    fn is_volume(id: Id) -> bool {
        matches!(id, Id::QVolume | Id::Volume | Id::HudVolume)
    }

    fn set_brightness(&mut self, pct: u8) -> Option<Action> {
        let pct = pct.clamp(MIN_PCT, 100);
        if self.cfg.brightness == pct {
            return None;
        }
        self.cfg.brightness = pct;
        Some(Action::Brightness(pct))
    }

    fn set_volume(&mut self, pct: u8) -> Option<Action> {
        let pct = pct.min(100);
        if self.cfg.volume == pct {
            return None;
        }
        self.cfg.volume = pct;
        Some(Action::Volume(pct))
    }

    fn slide(&mut self, id: Id, x: usize, sx: usize, sw: usize) -> Option<Action> {
        let rel = x.saturating_sub(sx).min(sw);
        if id == Id::HudVolume {
            // The indicator stays while a finger moves its level.
            self.vol_hud = libnx::time::now_ns() + VOL_HUD_NS;
        }
        if Self::is_volume(id) {
            self.set_volume((100 * rel / sw.max(1)) as u8)
        } else {
            self.set_brightness(
                (MIN_PCT as usize + (100 - MIN_PCT as usize) * rel / sw.max(1)) as u8,
            )
        }
    }

    pub(crate) fn overlay(&self) -> bool {
        self.modal.is_some() || self.sheet.is_some() || self.quick
    }

    pub fn touch(&mut self, code: u64, x: usize, y: usize) -> Option<Action> {
        self.focus = None;
        let hit = self.hit(x, y);
        match code {
            input::TOUCH_DOWN => {
                self.pressed = hit;
                if let Some(id) = hit.filter(|&id| Self::is_slider(id)) {
                    let z = self.zones.iter().find(|z| z.id == id)?;
                    let (sx, sw) = (z.x + 20, z.w.saturating_sub(40));
                    self.drag = Some((id, sx, sw));
                    return self.slide(id, x, sx, sw);
                }
                None
            }
            input::TOUCH_MOVE => {
                if let Some((id, sx, sw)) = self.drag {
                    return self.slide(id, x, sx, sw);
                }
                if self.pressed != hit {
                    self.pressed = None;
                }
                None
            }
            input::TOUCH_UP => {
                if let Some((id, _, _)) = self.drag.take() {
                    if id == Id::HudVolume {
                        self.vol_hud = libnx::time::now_ns() + VOL_HUD_NS;
                    }
                    self.pressed = None;
                    return Some(Action::Save);
                }
                let p = self.pressed.take()?;
                if Some(p) == hit {
                    self.tap(p)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn pad(&mut self, code: u64) -> Option<Action> {
        use input::*;
        let open = self.overlay();
        if self.flow.is_some() && !open {
            return self.flow_pad(code);
        }
        if self.page == LIBRARY && !open && self.starting.is_some() {
            return match code {
                BTN_B | BTN_HOME => Some(Action::Cancel),
                _ => None,
            };
        }
        if self.page == LIBRARY && !open {
            if let Some(a) = self.home_pad(code) {
                return a;
            }
        }
        if self.page == LIBRARY && !open && self.services.len() > 1 && !self.catalog_busy() {
            match code {
                BTN_L => return self.step_service(false),
                BTN_R => return self.step_service(true),
                _ => {}
            }
        }
        if let (Some(f), BTN_LEFT | BTN_RIGHT) = (self.focus, code) {
            if Self::is_slider(f) && self.zones.iter().any(|z| z.id == f) {
                if Self::is_volume(f) {
                    let v = self.cfg.volume;
                    let v = if code == BTN_LEFT {
                        v.saturating_sub(PAD_STEP)
                    } else {
                        v.saturating_add(PAD_STEP)
                    };
                    return self.set_volume(v).or(Some(Action::Save));
                }
                let v = self.cfg.brightness;
                let v = if code == BTN_LEFT {
                    v.saturating_sub(PAD_STEP)
                } else {
                    v.saturating_add(PAD_STEP)
                };
                return self.set_brightness(v).or(Some(Action::Save));
            }
        }
        match code {
            BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT => self.step_focus(code),
            BTN_A => match self.focus {
                Some(id) if self.zones.iter().any(|z| z.id == id) => return self.tap(id),
                _ => self.step_focus(code),
            },
            BTN_Y if !open => return self.set_theme(!self.cfg.light),
            BTN_B if self.modal.is_some() => return self.tap(Id::No),
            BTN_B if self.sheet.is_some() => self.sheet = None,
            BTN_B if self.quick => self.quick = false,
            BTN_B if self.scan.is_some() => self.scan = None,
            BTN_B | BTN_HOME if self.page != LIBRARY || open => {
                self.quick = false;
                self.sheet = None;
                self.go(LIBRARY);
            }
            BTN_MINUS | BTN_PLUS if self.modal.is_none() && self.sheet.is_none() => {
                self.toggle_quick()
            }
            BTN_L | BTN_ZL if !open => match self.page {
                SYSTEM => self.sys_tab ^= 1,
                CONTROLLERS => self.ctl_tab ^= 1,
                SETTINGS => {
                    let n = crate::settings::CATS.len();
                    self.cat = (self.cat + n - 1) % n;
                }
                LIBRARY => {
                    self.home = 0;
                    self.last_game = 0;
                }
                _ => {}
            },
            BTN_R | BTN_ZR if !open => match self.page {
                SYSTEM => self.sys_tab ^= 1,
                CONTROLLERS => self.ctl_tab ^= 1,
                SETTINGS => {
                    self.cat = (self.cat + 1) % crate::settings::CATS.len();
                }
                LIBRARY => {
                    self.home = self.games.len() + 5;
                }
                _ => {}
            },
            _ => {}
        }
        None
    }

    pub fn toggle_quick(&mut self) {
        self.quick = !self.quick;
        self.focus = self.quick.then_some(Id::QSleep);
    }

    /// Whether a touch at (x, y) lands on the overlay drawn over a game
    /// (the drawer takes them all).
    pub fn over_hit(&self, x: usize, y: usize) -> bool {
        self.quick || self.hit(x, y).is_some()
    }

    pub(crate) fn go(&mut self, page: usize) {
        self.page = page;
        self.focus = None;
        self.scan = None;
        if page == LOGS {
            self.unread = 0;
        }
        if page == USERS {
            self.usel = self.users.current.unwrap_or(0);
            self.want_saves();
        }
    }

    pub(crate) fn confirm(
        &mut self,
        title: &str,
        lines: [&str; 2],
        ok: &'static str,
        tone: Tone,
        a: Action,
    ) {
        self.modal = Some(Modal {
            title: title.into(),
            lines: [lines[0].into(), lines[1].into()],
            ok,
            tone,
            action: a,
        });
    }

    pub(crate) fn tap(&mut self, id: Id) -> Option<Action> {
        if self.flow.is_some() {
            if let Some(a) = self.flow_tap(id) {
                return a;
            }
        }
        if let Some(a) = self.users_tap(id) {
            return a;
        }
        match id {
            Id::AvatarHome if self.page == LIBRARY && self.users.active().is_some() => {
                self.go(USERS)
            }
            Id::AvatarHome if self.page == LIBRARY => return self.set_theme(!self.cfg.light),
            Id::AvatarHome => self.go(LIBRARY),
            Id::Scrim => {
                self.quick = false;
                self.sheet = None;
            }
            Id::StatusQuick => self.toggle_quick(),
            Id::Service(k) if !self.catalog_busy() => return Some(Action::Service(k)),
            Id::Service(_) => {}
            Id::Cancel => return Some(Action::Cancel),
            Id::Game(i) if self.home == i => return Some(Action::Launch(i)),
            Id::Game(i) => {
                self.home = i;
                self.last_game = i;
            }
            Id::Tile(k) => {
                self.home = self.games.len() + k;
                return self.open_tile(k);
            }
            Id::OpenPage(p) => self.go(p),
            Id::QSleep | Id::SleepNow => {
                self.quick = false;
                self.sheet = None;
                return Some(Action::Sleep);
            }
            Id::QSettings => {
                self.quick = false;
                self.go(SETTINGS);
            }
            Id::QBright | Id::Bright | Id::QVolume | Id::Volume | Id::Hud | Id::HudVolume => {}
            Id::Mute => {
                let next = if self.cfg.volume == 0 {
                    libnx::abi::proto::audio::DEFAULT_VOL
                } else {
                    0
                };
                return self.set_volume(next);
            }
            Id::PlayMusic => return Some(Action::Music),
            Id::PlayTone => return Some(Action::Tone),
            Id::Theme(light) => return self.set_theme(light),
            Id::QFlight | Id::Flight => {
                self.cfg.flight = !self.cfg.flight;
                return Some(Action::Flight(self.cfg.flight));
            }
            Id::Cat(i) => self.cat = i,
            Id::AutoSleep(i) => {
                self.cfg.sleep = i as u8;
                return Some(Action::Save);
            }
            Id::SysTab(i) => self.sys_tab = i,
            Id::CtlTab(i) => self.ctl_tab = i,
            Id::FanOpen => self.sheet = Some(Sheet::Fan),
            Id::CpuOpen => self.sheet = Some(Sheet::Cpu),
            Id::GpuOpen => self.sheet = Some(Sheet::Gpu),
            Id::SheetClose => self.sheet = None,
            Id::FanAuto if self.fan_manual().is_some() => return self.fan_set(fan::AUTO),
            Id::FanManual if self.fan_manual().is_none() => {
                let Seen::Value(f) = self.fan else {
                    return None;
                };
                return self.fan_set(f.percent);
            }
            Id::FanAuto | Id::FanManual => {}
            Id::FanDown | Id::FanUp => {
                let p = self.fan_manual()?;
                let q = match id {
                    Id::FanUp => (p / FAN_STEP + 1) * FAN_STEP,
                    _ => p.div_ceil(FAN_STEP).saturating_sub(1) * FAN_STEP,
                }
                .min(100);
                if q != p {
                    return self.fan_set(q);
                }
            }
            Id::CpuStep(_) | Id::CpuBoot if self.cpu_sent => {}
            Id::CpuStep(i) if self.cpu_step() == Some(cpufreq::STEPS_MHZ[i]) => {}
            Id::CpuBoot if self.cpu_step() == Some(cpufreq::BOOT) => {}
            Id::CpuStep(i) => {
                self.cpu_sent = true;
                return Some(Action::Cpu(cpufreq::STEPS_MHZ[i]));
            }
            Id::CpuBoot => {
                self.cpu_sent = true;
                return Some(Action::Cpu(cpufreq::BOOT));
            }
            Id::GpuStep(_) if self.gpu_sent => {}
            Id::GpuStep(i) => return self.gpu_step(i),
            Id::Reboot => {
                self.sheet = None;
                self.confirm(
                    "Restart the Switch?",
                    ["It goes back to RCM: inject nxk", "again from the PC."],
                    "Restart",
                    Tone::Warn,
                    Action::Power(power::REBOOT),
                )
            }
            Id::PowerOff => {
                self.sheet = None;
                self.confirm(
                    "Power off the Switch?",
                    [
                        "Power on goes to RCM (AutoRCM):",
                        "inject nxk again from the PC.",
                    ],
                    "Power off",
                    Tone::Bad,
                    Action::Power(power::OFF),
                )
            }
            Id::Leave => {
                let ssid = self.ssid.clone();
                self.modal = Some(Modal {
                    title: alloc::format!("Leave {}?", ssid),
                    lines: [
                        "Rejoining needs the shell:".into(),
                        alloc::format!("wifi connect {} PASSPHRASE", ssid),
                    ],
                    ok: "Leave",
                    tone: Tone::Warn,
                    action: Action::Leave,
                })
            }
            Id::Scan => {
                self.busy = true;
                return Some(Action::Scan);
            }
            Id::Retry => {
                self.busy = true;
                self.scan = None;
                return Some(Action::Retry);
            }
            Id::ScanBack => self.scan = None,
            Id::BtPair => return Some(Action::BtPair(!self.bt_pairing)),
            Id::BtForget(i) => {
                let (addr, name) = self.bt_pads.get(i)?.clone();
                self.modal = Some(Modal {
                    title: alloc::format!("Forget {}?", name),
                    lines: [
                        "Pair it again to use it:".into(),
                        "Pair a Controller, then its sync button.".into(),
                    ],
                    ok: "Forget",
                    tone: Tone::Warn,
                    action: Action::BtForget(addr),
                })
            }
            Id::No => self.modal = None,
            Id::Yes => {
                let a = self.modal.take().map(|m| m.action);
                if matches!(a, Some(Action::Gpu(..))) {
                    self.gpu_sent = true;
                }
                return a;
            }
            Id::UserRow(_)
            | Id::UserAdd
            | Id::UserSwitch
            | Id::UserUnlock
            | Id::UserRename
            | Id::UserLook
            | Id::UserPin
            | Id::UserDelete
            | Id::SaveErase(_)
            | Id::SavesPrev
            | Id::SavesNext
            | Id::Key(_)
            | Id::KeySpace
            | Id::KeyBack
            | Id::KeyShift
            | Id::Glyph(_)
            | Id::Color(_)
            | Id::Pin(_)
            | Id::PinBack
            | Id::FlowNext
            | Id::FlowBack
            | Id::FlowSkip
            | Id::LockOther => {}
            Id::LogSize => self.log_scale = 3 - self.log_scale,
            Id::LogClear => return Some(Action::ClearLogs),
            Id::TermSize => self.term_scale = 3 - self.term_scale,
        }
        None
    }

    pub(crate) fn open_power(&mut self) {
        self.sheet = Some(Sheet::Power);
    }

    /// The Power button held: the Power sheet over whatever is open, its
    /// first button (Sleep Mode) focused for the pad.
    pub(crate) fn power_menu(&mut self) {
        self.quick = false;
        self.modal = None;
        self.open_power();
        self.focus = Some(Id::SleepNow);
    }

    pub(crate) fn set_theme(&mut self, light: bool) -> Option<Action> {
        if self.cfg.light == light {
            return None;
        }
        self.cfg.light = light;
        theme::set(light);
        if let Some(i) = self.users.current {
            self.users.list[i].light = light;
            self.users.touch();
        }
        Some(Action::Save)
    }

    /// Whole frame into `s` (not yet on screen: `Screen::present`).
    pub fn draw(&mut self, s: &mut Screen, d: &Data, msgs: &Messages, term: &Term, now: u64) {
        let p = pal();
        self.zones.clear();
        self.wall = libnx::time::realtime_ns();
        self.fan = d.fan;
        self.cpu = d.cpu;
        self.gpu = d.gpu;
        s.clear(p.bg);
        self.sliding = false;
        if self.flow.is_some() {
            self.flow_screen(s, now);
        } else {
            match self.page {
                LIBRARY => self.home_page(s, d),
                SETTINGS => self.settings(s, d),
                NETWORK => self.network(s, d),
                SYSTEM => self.system(s, d),
                CONTROLLERS => self.controllers(s, d),
                USERS => self.users_page(s),
                TERMINAL => self.terminal(s, d, term, now),
                _ => self.logs(s, msgs),
            }
            self.top_bar(s, d);
            if self.page == LIBRARY && self.starting.is_some() {
                self.zones.clear();
                self.launch_card(s, now);
            }
        }
        if matches!(&self.toast, Some((_, end)) if now >= *end) {
            self.toast = None;
        }
        self.draw_toast(s);
        if self.quick {
            self.zones.clear();
            self.quick_panel(s, d);
        }
        if let Some(sh) = self.sheet {
            self.zones.clear();
            match sh {
                Sheet::Fan => self.fan_sheet(s),
                Sheet::Cpu => self.cpu_sheet(s),
                Sheet::Gpu => self.gpu_sheet(s),
                Sheet::Power => self.power_sheet(s),
            }
        }
        if self.modal.is_some() {
            self.zones.clear();
            self.modal(s);
        }
        self.draw_hud(s, now);
        // The home row shows its own selection; a sliding step has no zones yet.
        if (self.page == LIBRARY && self.flow.is_none() && !self.overlay()) || self.sliding {
            return;
        }
        self.focus_ring(s);
    }

    /// What shows over a game, on a transparent frame: toasts, the
    /// drawer, the volume indicator.
    pub fn draw_over(&mut self, s: &mut Screen, d: &Data, now: u64) {
        self.zones.clear();
        self.wall = libnx::time::realtime_ns();
        s.clear_clear();
        if matches!(&self.toast, Some((_, end)) if now >= *end) {
            self.toast = None;
        }
        self.draw_toast(s);
        if self.quick {
            self.zones.clear();
            self.quick_panel(s, d);
        }
        self.draw_hud(s, now);
        if self.quick {
            self.focus_ring(s);
        }
    }

    fn draw_toast(&self, s: &mut Screen) {
        let p = pal();
        if let Some((t, _)) = &self.toast {
            let w = font::width(Face::Body, t) + 56;
            let (x, y) = ((W - w) / 2, RULE_Y - 64);
            s.shadow(x, y, w, 48, 24, theme::BLACK);
            s.fill_round(x, y, w, 48, 24, p.text);
            s.label_c(W / 2, y + 24, t, Face::Body, p.bg);
        }
    }

    /// The volume indicator while it shows; touching its bar sets the level.
    fn draw_hud(&mut self, s: &mut Screen, now: u64) {
        if self.vol_hud == 0 {
            return;
        }
        if now >= self.vol_hud {
            if self.drag.is_none_or(|d| d.0 != Id::HudVolume) {
                self.vol_hud = 0;
                return;
            }
            self.vol_hud = now + VOL_HUD_NS;
        }
        let y = HEADER_RULE_Y + 16;
        let (x, w, h, bx, bw) = volume_hud(s, y, self.cfg.volume);
        self.zone(Id::Hud, x, y, w, h, h / 2);
        self.zone(Id::HudVolume, bx - 20, y, bw + 40, h, 8);
    }

    /// The gamepad's selection, as a ring around its zone.
    fn focus_ring(&mut self, s: &mut Screen) {
        let p = pal();
        if self
            .focus
            .is_some_and(|f| self.zones.iter().all(|z| z.id != f))
        {
            self.focus = self.nearest(self.focus_at.0, self.focus_at.1);
        }
        let ring = self
            .focus
            .and_then(|f| self.zones.iter().rev().find(|z| z.id == f))
            .map(|z| (z.x, z.y, z.w, z.h, z.r));
        if let Some((x, y, w, h, r)) = ring {
            self.focus_at = (x + w / 2, y + h / 2);
            s.ring_round(x - 4, y - 4, w + 8, h + 8, r + 4, 3, p.focus);
        }
    }

    /// Clock, Wi-Fi and power source ending at `xr`, centered on `cy`, on
    /// `bg`; returns where it starts.
    pub(crate) fn status(&self, s: &mut Screen, d: &Data, xr: usize, cy: usize, bg: u32) -> usize {
        let p = pal();
        let x = xr - 16;
        let level = match d.battery {
            Seen::Value(b) => Some(b.percent()),
            _ => None,
        };
        let charging = matches!(d.supply, Seen::Value(power::SUPPLY_EXTERNAL));
        let low = level.is_some_and(|l| l <= 15) && !charging;
        battery(s, x, cy, level, p.text, if low { p.bad } else { p.text });
        let mut left = x - 14;
        if let Some(l) = level {
            let t = alloc::format!("{}%", l);
            left -= 8 + s.label_r(
                left - 8,
                cy,
                &t,
                Face::Small,
                if low { p.bad } else { p.text },
            );
        }
        if charging {
            icon(s, Icon::Bolt, left - 12, cy, p.text, bg);
            left -= 24;
        }
        let x = left - 28;
        let (wt, _) = wifi_short(d);
        if self.cfg.flight || wt == "radio off" {
            icon(s, Icon::Plane, x, cy, p.text, bg);
        } else {
            let c = if wifi_connected(d).is_some() {
                p.text
            } else {
                p.faint
            };
            icon(s, Icon::Wifi, x, cy - 2, c, bg);
        }
        let x = x - 16 - 14;
        let t = clock(self.wall);
        x - s.label_r(x, cy, &t, Face::Small, p.text)
    }

    /// On the home screen: the player avatar and the status cluster.
    /// On an inner page: the page's icon and title above a horizontal rule.
    fn top_bar(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let cy = STATUS_Y;
        if self.page == LIBRARY {
            self.service_tabs(s);
            match self.users.active() {
                Some(u) => crate::avatar::avatar(s, 94, cy, 30, u.color, u.glyph),
                None => {
                    if let Some(img) = art::get(art::UI, art::AVATAR) {
                        s.image(&img, 64, cy - 30, 60, 60, 30);
                    }
                }
            }
            s.ring(94, cy, 31, 2, p.rule);
            self.zone(Id::AvatarHome, 64, cy - 30, 60, 60, 30);
            let from = self.status(s, d, W - 64, cy, p.bg);
            self.zone(Id::StatusQuick, from - 12, cy - 28, W - from + 12, 56, 8);
        } else if self.page == USERS {
            if let Some(u) = self.users.active() {
                crate::avatar::avatar(s, 78, cy, 20, u.color, u.glyph);
            }
            let tw = s.label(112, cy, "Users", Face::Title, p.text);
            self.zone(Id::AvatarHome, 46, cy - 32, tw + 80, 64, 8);
            s.fill(30, HEADER_RULE_Y, W - 60, 1, p.rule);
            let from = self.status(s, d, W - 58, cy, p.bg);
            self.zone(Id::StatusQuick, from - 12, cy - 28, W - from + 12, 56, 8);
        } else {
            let k = match self.page {
                SYSTEM => 0,
                NETWORK => 1,
                TERMINAL => 2,
                LOGS => 3,
                CONTROLLERS => 4,
                _ => 5,
            };
            if let Some(img) = art::get(art::UI, art::head(k)) {
                s.image(&img, 58, cy - 20, 40, 40, 6);
            }
            let tw = s.label(112, cy, crate::home::BUTTONS[k], Face::Title, p.text);
            self.zone(Id::AvatarHome, 46, cy - 32, tw + 80, 64, 8);
            s.fill(30, HEADER_RULE_Y, W - 60, 1, p.rule);
            let from = self.status(s, d, W - 58, cy, p.bg);
            self.zone(Id::StatusQuick, from - 12, cy - 28, W - from + 12, 56, 8);
        }
    }

    /// The bottom: rule, handheld icon, button hints ending at `xr`.
    pub(crate) fn hints(
        &self,
        s: &mut Screen,
        x0: usize,
        xr: usize,
        bg: u32,
        items: &[(&str, &str)],
    ) {
        let p = pal();
        let rx1 = if bg == p.bg { W - x0 } else { xr + 1 };
        s.fill(
            x0,
            RULE_Y,
            rx1 - x0,
            1,
            if bg == p.bg { p.rule } else { p.line },
        );
        icon(s, Icon::Handheld, x0 + 36, HINT_Y, p.text, bg);
        let mut x = xr;
        for (b, t) in items.iter().rev() {
            x -= s.label_r(x, HINT_Y, t, Face::Body, p.text);
            button_glyph(s, x - 19, HINT_Y, b, p.text, bg);
            x -= 19 + 11 + 30;
        }
    }

    /// The hint bar of a page: back and select.
    pub(crate) fn page_hints(&self, s: &mut Screen) {
        self.hints(s, 30, W - 54, pal().bg, &[("B", "Back"), ("A", "OK")]);
    }

    /// Button: filled in `tone`, or a raised one when `Plain`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn button(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        label: &str,
        tone: Tone,
        enabled: bool,
    ) {
        let p = pal();
        let down = enabled && self.pressed == Some(id);
        let (bg, fg) = if !enabled {
            (p.raised, p.faint)
        } else if tone == Tone::Plain {
            (press(p.raised, down), p.text)
        } else {
            let c = tone_color(tone);
            (if down { mix(c, theme::WHITE, 60) } else { c }, p.on_accent)
        };
        s.fill_round(x, y, w, h, 6, bg);
        let f = if h >= 52 { Face::Body } else { Face::Small };
        s.label_c(x + w / 2, y + h / 2, label, f, fg);
        if enabled {
            self.zone(id, x, y, w, h, 6);
        }
    }

    /// An option of a group: the chosen one in the accent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn chip(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        text: &str,
        chosen: bool,
        on: bool,
    ) {
        let p = pal();
        let down = on && self.pressed == Some(id);
        let (bg, fg) = if chosen {
            (p.accent, p.on_accent)
        } else if on {
            (press(p.raised, down), p.text)
        } else {
            (p.raised, p.faint)
        };
        s.fill_round(x, y, w, h, 6, bg);
        s.label_c(x + w / 2, y + h / 2, text, Face::Small, fg);
        if on {
            self.zone(id, x, y, w, h, 6);
        }
    }

    /// A row that opens a sheet: label, value, chevron.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_row(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        w: usize,
        y: usize,
        label: &str,
        text: &str,
        c: u32,
    ) {
        let p = pal();
        let bg = press(p.raised, self.pressed == Some(id));
        s.fill_round(x, y, w, 48, 6, bg);
        s.label(x + 16, y + 24, label, Face::Small, p.dim);
        s.label_r(x + w - 40, y + 24, text, Face::Small, c);
        icon(s, Icon::Chevron, x + w - 20, y + 24, p.dim, bg);
        self.zone(id, x, y, w, 48, 6);
    }

    /// A card that opens something when tapped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tile_card(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        title: &str,
    ) {
        crate::widgets::titled(s, x, y, w, h, title);
        if self.pressed == Some(id) {
            s.ring_round(x, y, w, h, 8, 2, pal().accent);
        }
        self.zone(id, x, y, w, h, 8);
    }

    pub(crate) fn sleep_minutes(&self) -> u8 {
        SLEEP_MIN[self.cfg.sleep as usize % SLEEP_MIN.len()]
    }
}
