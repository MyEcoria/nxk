//! input : evenements d'entree (ecran tactile, clavier).
//!
//! L'ecran tactile (FTM3BD56 de STMicro, I2C3 adresse 0x49) est interroge a
//! 100 Hz : sa pile d'evenements se lit par la commande 0x85. Les autres
//! pilotes (clavier USB) deposent leurs evenements par INJECT. Les clients
//! attendent le suivant par READ (reponse differee, jeton de reponse).

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
/// Lecture de registre : 0x0023 = nombre d'evenements en attente (x2).
const READ_REG: u8 = 0xB6;
const POLL_NS: u64 = 10_000_000;

#[derive(Clone, Copy)]
struct Event([u64; 5]);

fn reply(e: Event) -> Msg {
    let mut m = Msg::new(0, 5, 0);
    m.mr[..5].copy_from_slice(&e.0);
    m
}

/// Evenements en attente dans le controleur tactile ; rend vrai si un doigt
/// reste pose apres eux.
fn poll_touch(i2c: &I2c, out: &mut VecDeque<Event>, mut down: bool) -> bool {
    let mut count = [0u8; 2];
    if i2c.transfer(3, TOUCH_ADDR, &[READ_REG, 0x00, 0x23], &mut count).is_err() {
        return down;
    }
    for _ in 0..(count[1] >> 1).min(32) {
        let mut ev = [0u8; 8];
        if i2c.transfer(3, TOUCH_ADDR, &[READ_ONE_EVENT], &mut ev).is_err() {
            return down;
        }
        let code = match ev[0] & 0x0F {
            0x03 => proto::TOUCH_DOWN,
            0x05 => proto::TOUCH_MOVE,
            0x04 => proto::TOUCH_UP,
            _ => continue, // autres evenements (controleur pret, etat...)
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
    let i2c = I2c::connect().expect("input : service i2c");
    let touch = i2c.transfer(3, TOUCH_ADDR, &[MS_MT_SENSE_ON], &mut []).is_ok();
    let ep = drv::publish("input").expect("input : publication");
    println!("[input] ecran tactile {}", if touch { "pret" } else { "absent" });

    let mut queue: VecDeque<Event> = VecDeque::new();
    // Lecteurs en attente : jeton de reponse et echeance (0 : aucune).
    let mut readers: Vec<(u64, u64)> = Vec::new();
    let mut down = false;
    loop {
        // Le controleur n'est interroge que si quelqu'un attend un evenement
        // ou qu'un doigt est pose : sans lecteur, aucun reveil periodique (le
        // coeur reste au repos, rien ne tourne pour un serveur sans ecran).
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
        // Distribution aux lecteurs en attente, puis delais echus.
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
        // Evenements sans lecteur : on n'en garde qu'un nombre raisonnable.
        while queue.len() > 256 {
            queue.pop_front();
        }
    }
}
