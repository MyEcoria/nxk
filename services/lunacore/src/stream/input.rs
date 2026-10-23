//! Input events (keyboard, mouse, gamepad) encoded as `dcv.input` messages.
//!
//! Pure module: no I/O and no windowing. A frontend turns its own events into
//! [`InputEvent`], and [`InputEvent::to_message`] builds the `ClientMessage` to
//! send on the `input` channel (framing is done by the session layer).
//!
//! # Provisional mappings
//!
//! The format comes from the Android client (`docs/PROTOCOL.md` §7). It has
//! never been checked against the server with a non-Android client, so these
//! stay provisional until a real session confirms them:
//!
//! - key names ([`SpecialKey::name`]) and the use of characters for letters,
//!   digits and punctuation instead of physical keys;
//! - button codes of [`MouseButton`] and the scroll scale and sign of
//!   [`MouseEvent::Scroll`];
//! - the two's complement encoding of negative stick values in `uint32` fields,
//!   the XInput button layout and the inverted stick Y axis (up is positive);
//! - the `til` metadata (hardware class, event type, injection id) that the
//!   Android client fills for gamepads is not reproduced.

use prost::Message;

use crate::proto::input::{
    self as pb, client_message::Msg, pointer_axis_event::Axis, ClientMessage, GamePadType,
    KeyboardLocation,
};

/// Android client scroll unit: one wheel notch is 120 units.
const SCROLL_UNITS_PER_NOTCH: i32 = 120;

/// # Position of a key on the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyLocation {
    Standard,
    Left,
    Right,
    Numpad,
}

impl KeyLocation {
    fn to_proto(self) -> KeyboardLocation {
        match self {
            Self::Standard => KeyboardLocation::Standard,
            Self::Left => KeyboardLocation::Left,
            Self::Right => KeyboardLocation::Right,
            Self::Numpad => KeyboardLocation::Numpad,
        }
    }
}

/// # Keys sent by name instead of by character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialKey {
    Shift,
    Control,
    Alt,
    Meta,
    CapsLock,
    NumLock,
    Tab,
    Enter,
    Backspace,
    Delete,
    Insert,
    Escape,
    ContextMenu,
    PrintScreen,
    ScrollLock,
    Pause,
    Home,
    End,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    MediaPlay,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    MediaFastForward,
}

impl SpecialKey {
    /// Every special key, in declaration order.
    pub const ALL: [Self; 41] = [
        Self::Shift,
        Self::Control,
        Self::Alt,
        Self::Meta,
        Self::CapsLock,
        Self::NumLock,
        Self::Tab,
        Self::Enter,
        Self::Backspace,
        Self::Delete,
        Self::Insert,
        Self::Escape,
        Self::ContextMenu,
        Self::PrintScreen,
        Self::ScrollLock,
        Self::Pause,
        Self::Home,
        Self::End,
        Self::PageUp,
        Self::PageDown,
        Self::F1,
        Self::F2,
        Self::F3,
        Self::F4,
        Self::F5,
        Self::F6,
        Self::F7,
        Self::F8,
        Self::F9,
        Self::F10,
        Self::F11,
        Self::F12,
        Self::ArrowLeft,
        Self::ArrowRight,
        Self::ArrowUp,
        Self::ArrowDown,
        Self::MediaPlay,
        Self::MediaStop,
        Self::MediaTrackNext,
        Self::MediaTrackPrevious,
        Self::MediaFastForward,
    ];

    /// # Name expected by the server (provisional table).
    ///
    /// DOM `KeyboardEvent.key` style, as sent by the Android client.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Shift => "Shift",
            Self::Control => "Control",
            Self::Alt => "Alt",
            Self::Meta => "OS",
            Self::CapsLock => "CapsLock",
            Self::NumLock => "NumLock",
            Self::Tab => "Tab",
            Self::Enter => "Enter",
            Self::Backspace => "Backspace",
            Self::Delete => "Delete",
            Self::Insert => "Insert",
            Self::Escape => "Escape",
            Self::ContextMenu => "ContextMenu",
            Self::PrintScreen => "PrintScreen",
            Self::ScrollLock => "ScrollLock",
            Self::Pause => "Pause",
            Self::Home => "Home",
            Self::End => "End",
            Self::PageUp => "PageUp",
            Self::PageDown => "PageDown",
            Self::F1 => "F1",
            Self::F2 => "F2",
            Self::F3 => "F3",
            Self::F4 => "F4",
            Self::F5 => "F5",
            Self::F6 => "F6",
            Self::F7 => "F7",
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
            Self::ArrowLeft => "ArrowLeft",
            Self::ArrowRight => "ArrowRight",
            Self::ArrowUp => "ArrowUp",
            Self::ArrowDown => "ArrowDown",
            Self::MediaPlay => "MediaPlay",
            Self::MediaStop => "MediaStop",
            Self::MediaTrackNext => "MediaTrackNext",
            Self::MediaTrackPrevious => "MediaTrackPrevious",
            Self::MediaFastForward => "MediaFastForward",
        }
    }

    /// # Finds a special key from its protocol name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }
}

/// # A key, as the server identifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Printable character produced by the key (case follows Shift).
    Character(char),
    /// Numeric keypad digit or operator (`0-9 / * - + . , = ( )`).
    Numpad(char),
    /// Named key; left/right variants of modifiers use [`KeyLocation`].
    Special(SpecialKey, KeyLocation),
}

/// # A key press or release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
}

/// # Mouse button with its protocol code (provisional).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl MouseButton {
    /// Code carried by `PointerButtonPressEvent` and `PointerButtonReleaseEvent`.
    pub const fn code(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::Back => 4,
            Self::Forward => 5,
        }
    }
}

/// # Scroll wheel axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    Horizontal,
    Vertical,
}

/// # A mouse event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    /// Absolute position in stream pixels.
    Move {
        x: u32,
        y: u32,
    },
    /// Relative motion, used when the server asks for relative mode.
    MoveRelative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel notches: positive is up (vertical) or right (horizontal).
    Scroll {
        axis: ScrollAxis,
        notches: i32,
    },
}

/// # XInput button bit mask of a gamepad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamePadButtons(pub u32);

impl GamePadButtons {
    pub const DPAD_UP: Self = Self(0x0001);
    pub const DPAD_DOWN: Self = Self(0x0002);
    pub const DPAD_LEFT: Self = Self(0x0004);
    pub const DPAD_RIGHT: Self = Self(0x0008);
    pub const START: Self = Self(0x0010);
    pub const BACK: Self = Self(0x0020);
    pub const LEFT_THUMB: Self = Self(0x0040);
    pub const RIGHT_THUMB: Self = Self(0x0080);
    pub const LEFT_SHOULDER: Self = Self(0x0100);
    pub const RIGHT_SHOULDER: Self = Self(0x0200);
    pub const A: Self = Self(0x1000);
    pub const B: Self = Self(0x2000);
    pub const X: Self = Self(0x4000);
    pub const Y: Self = Self(0x8000);

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for GamePadButtons {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// # Gamepad thumbstick.
///
/// Axes range over `i16`, with up and right positive (XInput convention).
/// `sensitivity` goes from -10 to 10 in the Android client; 0 is neutral.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stick {
    pub x: i16,
    pub y: i16,
    pub sensitivity: i32,
}

/// # Gamepad family reported to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamePadKind {
    Xbox360,
    DualShock4,
}

impl GamePadKind {
    fn to_proto(self) -> GamePadType {
        match self {
            Self::Xbox360 => GamePadType::X360,
            Self::DualShock4 => GamePadType::Ds4,
        }
    }
}

/// # Full state of a gamepad at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamePadState {
    pub buttons: GamePadButtons,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_stick: Stick,
    pub right_stick: Stick,
}

/// # A gamepad state update.
///
/// `packet_number` is a per-device counter that starts at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GamePadEvent {
    pub device_id: u32,
    pub packet_number: u32,
    pub kind: GamePadKind,
    pub state: GamePadState,
}

/// # A gamepad being plugged in or removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GamePadConnection {
    pub device_id: u32,
    pub connected: bool,
    pub kind: GamePadKind,
}

/// # Any input event the client can send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    GamePad(GamePadEvent),
    GamePadConnection(GamePadConnection),
}

impl From<KeyEvent> for InputEvent {
    fn from(event: KeyEvent) -> Self {
        Self::Key(event)
    }
}

impl From<MouseEvent> for InputEvent {
    fn from(event: MouseEvent) -> Self {
        Self::Mouse(event)
    }
}

impl From<GamePadEvent> for InputEvent {
    fn from(event: GamePadEvent) -> Self {
        Self::GamePad(event)
    }
}

impl From<GamePadConnection> for InputEvent {
    fn from(event: GamePadConnection) -> Self {
        Self::GamePadConnection(event)
    }
}

impl InputEvent {
    /// # Builds the `ClientMessage` for this event.
    pub fn to_message(&self) -> ClientMessage {
        let msg = match self {
            Self::Key(event) => key_msg(event),
            Self::Mouse(event) => mouse_msg(event),
            Self::GamePad(event) => Msg::GamePadEvent(gamepad_event(event)),
            Self::GamePadConnection(event) => {
                Msg::GamePadConnectionEvent(gamepad_connection(event))
            }
        };
        ClientMessage {
            msg: Some(msg),
            ..Default::default()
        }
    }

    /// # Serializes the event as protobuf bytes, without any channel framing.
    pub fn encode(&self) -> Vec<u8> {
        self.to_message().encode_to_vec()
    }
}

fn key_msg(event: &KeyEvent) -> Msg {
    let (character, name, location) = match event.key {
        Key::Character(c) => (Some(c.to_string()), None, KeyLocation::Standard),
        Key::Numpad(c) => (Some(c.to_string()), None, KeyLocation::Numpad),
        Key::Special(key, location) => (None, Some(key.name().to_owned()), location),
    };
    let location = Some(location.to_proto() as i32);
    let r#virtual = Some(false);
    match (character, name, event.pressed) {
        (Some(character), _, true) => Msg::KeyDownEvent(pb::KeyDownEvent {
            character: Some(character),
            location,
            r#virtual,
            til: None,
        }),
        (Some(character), _, false) => Msg::KeyUpEvent(pb::KeyUpEvent {
            character: Some(character),
            location,
            r#virtual,
            til: None,
        }),
        (None, name, true) => Msg::SpecialKeyDownEvent(pb::SpecialKeyDownEvent {
            name,
            location,
            r#virtual,
            til: None,
        }),
        (None, name, false) => Msg::SpecialKeyUpEvent(pb::SpecialKeyUpEvent {
            name,
            location,
            r#virtual,
            til: None,
        }),
    }
}

fn mouse_msg(event: &MouseEvent) -> Msg {
    match *event {
        MouseEvent::Move { x, y } => Msg::PointerMotionEvent(pb::PointerMotionEvent {
            x: Some(x),
            y: Some(y),
            request_ack: None,
            til: None,
        }),
        MouseEvent::MoveRelative { dx, dy } => {
            Msg::PointerRelativeMotionEvent(pb::PointerRelativeMotionEvent {
                dx: Some(dx),
                dy: Some(dy),
                til: None,
            })
        }
        MouseEvent::Button {
            button,
            pressed: true,
        } => Msg::PointerButtonPressEvent(pb::PointerButtonPressEvent {
            button: Some(button.code()),
            til: None,
        }),
        MouseEvent::Button {
            button,
            pressed: false,
        } => Msg::PointerButtonReleaseEvent(pb::PointerButtonReleaseEvent {
            button: Some(button.code()),
            til: None,
        }),
        MouseEvent::Scroll { axis, notches } => {
            let (axis, sign) = match axis {
                ScrollAxis::Horizontal => (Axis::XAxis, 1),
                ScrollAxis::Vertical => (Axis::YAxis, -1),
            };
            Msg::PointerAxisEvent(pb::PointerAxisEvent {
                axis: Some(axis as i32),
                value: Some(notches.saturating_mul(SCROLL_UNITS_PER_NOTCH * sign)),
                til: None,
            })
        }
    }
}

/// Signed axis values travel as the raw 32-bit pattern, like the Android client.
fn axis_bits(value: i16) -> u32 {
    i32::from(value) as u32
}

fn gamepad_event(event: &GamePadEvent) -> pb::GamePadEvent {
    let state = &event.state;
    pb::GamePadEvent {
        device_id: Some(event.device_id),
        dw_packet_number: Some(event.packet_number),
        w_buttons: Some(state.buttons.0),
        b_left_trigger: Some(u32::from(state.left_trigger)),
        b_right_trigger: Some(u32::from(state.right_trigger)),
        s_thumb_lx: Some(axis_bits(state.left_stick.x)),
        s_thumb_ly: Some(axis_bits(state.left_stick.y)),
        s_thumb_rx: Some(axis_bits(state.right_stick.x)),
        s_thumb_ry: Some(axis_bits(state.right_stick.y)),
        device_type: Some(event.kind.to_proto() as i32),
        til: None,
        thumb_sensitivity_l: Some(state.left_stick.sensitivity),
        thumb_sensitivity_r: Some(state.right_stick.sensitivity),
    }
}

fn gamepad_connection(event: &GamePadConnection) -> pb::GamePadConnectionEvent {
    pb::GamePadConnectionEvent {
        device_id: Some(event.device_id),
        status: Some(u32::from(!event.connected)),
        device_type: Some(event.kind.to_proto() as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn round_trip(event: impl Into<InputEvent>) -> ClientMessage {
        let event = event.into();
        let decoded = ClientMessage::decode(event.encode().as_slice()).expect("decodes");
        assert_eq!(decoded, event.to_message());
        decoded
    }

    fn key(key: Key, pressed: bool) -> KeyEvent {
        KeyEvent { key, pressed }
    }

    #[test]
    fn character_key_round_trip() {
        let Some(Msg::KeyDownEvent(down)) = round_trip(key(Key::Character('w'), true)).msg else {
            panic!("expected key down");
        };
        assert_eq!(down.character.as_deref(), Some("w"));
        assert_eq!(down.location, Some(KeyboardLocation::Standard as i32));
        assert_eq!(down.r#virtual, Some(false));

        let Some(Msg::KeyUpEvent(up)) = round_trip(key(Key::Character('é'), false)).msg else {
            panic!("expected key up");
        };
        assert_eq!(up.character.as_deref(), Some("é"));
    }

    #[test]
    fn numpad_key_uses_numpad_location() {
        let Some(Msg::KeyDownEvent(down)) = round_trip(key(Key::Numpad('7'), true)).msg else {
            panic!("expected key down");
        };
        assert_eq!(down.character.as_deref(), Some("7"));
        assert_eq!(down.location, Some(KeyboardLocation::Numpad as i32));
    }

    #[test]
    fn special_key_round_trip() {
        let event = key(Key::Special(SpecialKey::Control, KeyLocation::Right), true);
        let Some(Msg::SpecialKeyDownEvent(down)) = round_trip(event).msg else {
            panic!("expected special key down");
        };
        assert_eq!(down.name.as_deref(), Some("Control"));
        assert_eq!(down.location, Some(KeyboardLocation::Right as i32));

        let event = key(
            Key::Special(SpecialKey::Escape, KeyLocation::Standard),
            false,
        );
        let Some(Msg::SpecialKeyUpEvent(up)) = round_trip(event).msg else {
            panic!("expected special key up");
        };
        assert_eq!(up.name.as_deref(), Some("Escape"));
    }

    #[test]
    fn numpad_enter_is_enter_at_numpad_location() {
        let event = key(Key::Special(SpecialKey::Enter, KeyLocation::Numpad), true);
        let Some(Msg::SpecialKeyDownEvent(down)) = round_trip(event).msg else {
            panic!("expected special key down");
        };
        assert_eq!(down.name.as_deref(), Some("Enter"));
        assert_eq!(down.location, Some(KeyboardLocation::Numpad as i32));
    }

    #[test]
    fn key_name_table_matches_the_documented_names() {
        let expected = [
            (SpecialKey::Meta, "OS"),
            (SpecialKey::Control, "Control"),
            (SpecialKey::Backspace, "Backspace"),
            (SpecialKey::ContextMenu, "ContextMenu"),
            (SpecialKey::PageDown, "PageDown"),
            (SpecialKey::F1, "F1"),
            (SpecialKey::F12, "F12"),
            (SpecialKey::ArrowUp, "ArrowUp"),
            (SpecialKey::MediaFastForward, "MediaFastForward"),
        ];
        for (key, name) in expected {
            assert_eq!(key.name(), name);
        }
    }

    #[test]
    fn key_names_are_unique_and_reversible() {
        let names: HashSet<_> = SpecialKey::ALL.iter().map(|key| key.name()).collect();
        assert_eq!(names.len(), SpecialKey::ALL.len());
        for key in SpecialKey::ALL {
            assert_eq!(SpecialKey::from_name(key.name()), Some(key));
        }
        assert_eq!(SpecialKey::from_name("w"), None);
    }

    #[test]
    fn function_keys_cover_f1_to_f12() {
        let names: Vec<_> = SpecialKey::ALL
            .iter()
            .map(|key| key.name())
            .filter(|name| name.starts_with('F') && name[1..].parse::<u8>().is_ok())
            .collect();
        let expected: Vec<_> = (1..=12).map(|n| format!("F{n}")).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn mouse_move_round_trip() {
        let Some(Msg::PointerMotionEvent(motion)) =
            round_trip(MouseEvent::Move { x: 1920, y: 1080 }).msg
        else {
            panic!("expected motion");
        };
        assert_eq!((motion.x, motion.y), (Some(1920), Some(1080)));

        let Some(Msg::PointerRelativeMotionEvent(motion)) =
            round_trip(MouseEvent::MoveRelative { dx: -5, dy: 12 }).msg
        else {
            panic!("expected relative motion");
        };
        assert_eq!((motion.dx, motion.dy), (Some(-5), Some(12)));
    }

    #[test]
    fn mouse_button_codes_and_round_trip() {
        let codes = [
            (MouseButton::Left, 1),
            (MouseButton::Middle, 2),
            (MouseButton::Right, 3),
            (MouseButton::Back, 4),
            (MouseButton::Forward, 5),
        ];
        for (button, code) in codes {
            let Some(Msg::PointerButtonPressEvent(press)) = round_trip(MouseEvent::Button {
                button,
                pressed: true,
            })
            .msg
            else {
                panic!("expected press");
            };
            assert_eq!(press.button, Some(code));
            let Some(Msg::PointerButtonReleaseEvent(release)) = round_trip(MouseEvent::Button {
                button,
                pressed: false,
            })
            .msg
            else {
                panic!("expected release");
            };
            assert_eq!(release.button, Some(code));
        }
    }

    #[test]
    fn scroll_scales_by_120_and_inverts_vertical() {
        let scroll = |axis, notches| {
            let Some(Msg::PointerAxisEvent(event)) =
                round_trip(MouseEvent::Scroll { axis, notches }).msg
            else {
                panic!("expected axis event");
            };
            (event.axis, event.value)
        };
        assert_eq!(
            scroll(ScrollAxis::Vertical, 1),
            (Some(Axis::YAxis as i32), Some(-120))
        );
        assert_eq!(
            scroll(ScrollAxis::Vertical, -2),
            (Some(Axis::YAxis as i32), Some(240))
        );
        assert_eq!(
            scroll(ScrollAxis::Horizontal, 3),
            (Some(Axis::XAxis as i32), Some(360))
        );
        assert_eq!(scroll(ScrollAxis::Vertical, i32::MIN).1, Some(i32::MAX));
    }

    #[test]
    fn gamepad_button_bits_follow_xinput() {
        let buttons = GamePadButtons::A | GamePadButtons::DPAD_LEFT;
        assert_eq!(buttons.0, 0x1004);
        assert!(buttons.contains(GamePadButtons::A));
        assert!(!buttons.contains(GamePadButtons::B));
        assert_eq!(GamePadButtons::Y.0, 0x8000);
        assert_eq!(GamePadButtons::LEFT_SHOULDER.0, 0x100);
    }

    #[test]
    fn gamepad_event_round_trip() {
        let event = GamePadEvent {
            device_id: 2,
            packet_number: 7,
            kind: GamePadKind::Xbox360,
            state: GamePadState {
                buttons: GamePadButtons::A | GamePadButtons::START,
                left_trigger: 255,
                right_trigger: 10,
                left_stick: Stick {
                    x: i16::MIN,
                    y: 32767,
                    sensitivity: -3,
                },
                right_stick: Stick {
                    x: -1,
                    y: 0,
                    sensitivity: 10,
                },
            },
        };
        let Some(Msg::GamePadEvent(pad)) = round_trip(event).msg else {
            panic!("expected gamepad event");
        };
        assert_eq!(pad.device_id, Some(2));
        assert_eq!(pad.dw_packet_number, Some(7));
        assert_eq!(pad.w_buttons, Some(0x1010));
        assert_eq!(pad.b_left_trigger, Some(255));
        assert_eq!(pad.b_right_trigger, Some(10));
        assert_eq!(pad.s_thumb_lx.map(|v| v as i32), Some(-32768));
        assert_eq!(pad.s_thumb_ly, Some(32767));
        assert_eq!(pad.s_thumb_rx, Some(u32::MAX));
        assert_eq!(pad.s_thumb_ry, Some(0));
        assert_eq!(pad.device_type, Some(GamePadType::X360 as i32));
        assert_eq!(pad.thumb_sensitivity_l, Some(-3));
        assert_eq!(pad.thumb_sensitivity_r, Some(10));
    }

    #[test]
    fn gamepad_connection_round_trip() {
        let connection = |connected| GamePadConnection {
            device_id: 1,
            connected,
            kind: GamePadKind::DualShock4,
        };
        let Some(Msg::GamePadConnectionEvent(on)) = round_trip(connection(true)).msg else {
            panic!("expected connection event");
        };
        assert_eq!(on.status, Some(0));
        assert_eq!(on.device_type, Some(GamePadType::Ds4 as i32));
        let Some(Msg::GamePadConnectionEvent(off)) = round_trip(connection(false)).msg else {
            panic!("expected connection event");
        };
        assert_eq!(off.status, Some(1));
        assert_eq!(off.device_id, Some(1));
    }
}
