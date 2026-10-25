//! The console's controls as the Xbox 360 pad Luna's servers expect, by
//! label: the button marked A is the game's A, B is B, X is X, Y is Y (a
//! mapping by position would put Nintendo's A, on the right, on Xbox's B,
//! and the player reads the prompt "A" on the button marked A); ZL and ZR
//! are full triggers; `+` is Start, `-` is Back. Sticks come dead-zoned
//! from the driver.

use lunacore::stream::input::{GamePadButtons as B, GamePadState, Stick};
use nxk_abi::proto::input as inp;

/// Applies one input event (`input::READ`'s words) to `pad`; true if it
/// changed.
pub fn apply(pad: &mut GamePadState, ev: &[u64; 6]) -> bool {
    let before = *pad;
    match ev[0] {
        inp::KIND_BUTTON => {
            let down = ev[2] != 0;
            match ev[1] {
                inp::BTN_ZL => pad.left_trigger = if down { 255 } else { 0 },
                inp::BTN_ZR => pad.right_trigger = if down { 255 } else { 0 },
                code => {
                    if let Some(b) = button(code) {
                        pad.buttons = if down {
                            B(pad.buttons.0 | b.0)
                        } else {
                            B(pad.buttons.0 & !b.0)
                        };
                    }
                }
            }
        }
        inp::KIND_STICK => {
            let axis = |w: u64| (w as i64).clamp(-inp::STICK_MAX, inp::STICK_MAX) as i16;
            let s = Stick {
                x: axis(ev[3]),
                y: axis(ev[4]),
                sensitivity: 0,
            };
            match ev[1] {
                inp::STICK_L => pad.left_stick = s,
                inp::STICK_R => pad.right_stick = s,
                _ => {}
            }
        }
        _ => {}
    }
    *pad != before
}

fn button(code: u64) -> Option<B> {
    Some(match code {
        inp::BTN_A => B::A,
        inp::BTN_B => B::B,
        inp::BTN_X => B::X,
        inp::BTN_Y => B::Y,
        inp::BTN_L => B::LEFT_SHOULDER,
        inp::BTN_R => B::RIGHT_SHOULDER,
        inp::BTN_PLUS => B::START,
        inp::BTN_MINUS => B::BACK,
        inp::BTN_LSTICK => B::LEFT_THUMB,
        inp::BTN_RSTICK => B::RIGHT_THUMB,
        inp::BTN_UP => B::DPAD_UP,
        inp::BTN_DOWN => B::DPAD_DOWN,
        inp::BTN_LEFT => B::DPAD_LEFT,
        inp::BTN_RIGHT => B::DPAD_RIGHT,
        _ => return None,
    })
}
