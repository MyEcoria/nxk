//! input: touchscreen (polled at 100 Hz) and keyboard events.
//! Clients wait via READ; drivers inject via INJECT.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ETIMEDOUT};
use libnx::abi::proto::input as proto;
use libnx::drv;
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg, Received};
use libnx::{println, time};

const TOUCH_ADDR: u8 = 0x49;
const READ_ONE_EVENT: u8 = 0x85;
const MS_MT_SENSE_ON: u8 = 0x93;
/// Register read: 0x0023 = pending event count (x2).
const READ_REG: u8 = 0xB6;
const POLL_NS: u64 = 10_000_000;

#[derive(Clone, Copy)]
struct Event([u64; 5]);

fn reply(e: Event) -> Msg {
    let mut m = Msg::new(0, 5, 0);
    m.mr[..5].copy_from_slice(&e.0);
    m
}

/// Pending events in the touch controller; true if a finger is still
/// down afterwards.
fn poll_touch(i2c: &I2c, out: &mut VecDeque<Event>, mut down: bool) -> bool {
    let mut count = [0u8; 2];
    if i2c
        .transfer(3, TOUCH_ADDR, &[READ_REG, 0x00, 0x23], &mut count)
        .is_err()
    {
        return down;
    }
    for _ in 0..(count[1] >> 1).min(32) {
        let mut ev = [0u8; 8];
        if i2c
            .transfer(3, TOUCH_ADDR, &[READ_ONE_EVENT], &mut ev)
            .is_err()
        {
            return down;
        }
        let code = match ev[0] & 0x0F {
            0x03 => proto::TOUCH_DOWN,
            0x05 => proto::TOUCH_MOVE,
            0x04 => proto::TOUCH_UP,
            _ => continue, // other events (controller ready, state...)
        };
        let finger = (ev[0] >> 4) as u64;
        let x = (ev[1] as u64) << 4 | (ev[3] >> 4) as u64;
        let y = (ev[2] as u64) << 4 | (ev[3] & 0xF) as u64;
        down = code != proto::TOUCH_UP;
        out.push_back(Event([proto::KIND_TOUCH, code, finger, x, y]));
    }
    down
}

libnx::entry!(main);

fn main() -> i32 {
    let i2c = I2c::connect().expect("input: i2c service");
    let touch = i2c
        .transfer(3, TOUCH_ADDR, &[MS_MT_SENSE_ON], &mut [])
        .is_ok();
    let ep = drv::publish("input").expect("input: publish");
    println!(
        "[input] touchscreen {}",
        if touch { "ready" } else { "missing" }
    );

    let mut queue: VecDeque<Event> = VecDeque::new();
    // Waiting readers: reply token and deadline (0: none).
    let mut readers: Vec<(u64, u64)> = Vec::new();
    let mut down = false;
    loop {
        // Poll only while a reader waits or a finger is down (else idle).
        let polling = touch && (!readers.is_empty() || down);
        let rx: Option<Received> = match ipc::recv_timeout(ep, if polling { POLL_NS } else { 0 }) {
            Ok(r) => Some(r),
            Err(e) if e == -ETIMEDOUT => None,
            Err(_) => None,
        };
        if touch {
            down = poll_touch(&i2c, &mut queue, down);
        }
        if let Some(rx) = rx {
            match rx.msg.label() {
                proto::READ => match queue.pop_front() {
                    Some(e) => {
                        let _ = ipc::reply(&reply(e));
                    }
                    None => {
                        let t = rx.msg.mr[0];
                        let deadline = if t == 0 { 0 } else { time::now_ns() + t };
                        readers.push((rx.token, deadline));
                    }
                },
                proto::INJECT => {
                    let mut e = [0u64; 5];
                    e.copy_from_slice(&rx.msg.mr[..5]);
                    queue.push_back(Event(e));
                    let _ = ipc::reply(&Msg::new(0, 0, 0));
                }
                _ => {
                    let _ = ipc::reply(&Msg::error(-EINVAL));
                }
            }
        }
        // Deliver to waiting readers, then expired deadlines.
        while !readers.is_empty() {
            let Some(e) = queue.pop_front() else { break };
            let (tok, _) = readers.remove(0);
            let _ = ipc::reply_to(tok, &reply(e));
        }
        let now = time::now_ns();
        readers.retain(|&(tok, d)| {
            if d != 0 && now >= d {
                let _ = ipc::reply_to(tok, &Msg::error(-ETIMEDOUT));
                false
            } else {
                true
            }
        });
        // Events with no reader: keep only a bounded number.
        while queue.len() > 256 {
            queue.pop_front();
        }
    }
}
