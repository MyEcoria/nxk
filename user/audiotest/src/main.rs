//! audiotest: a quiet 440 Hz sine through a client ring of the `audio`
//! service, as a game or Luna's player uses it. `audiotest [AHEAD [MS
//! [LEVEL]]]`: AHEAD frames wanted ahead of the speakers (0: the
//! service's default 4096; 512: 4 DMA periods of 128), MS of sound (2000),
//! LEVEL 0..100 (10). Afterwards `audio status` shows how the pump fared.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use libnx::abi::err;
use libnx::abi::proto::audio;
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{env, ns, println, sys, time};

libnx::entry!(main);

const FRAMES: usize = 4096;
/// Frames kept in the ring, as a player would (43 ms).
const KEEP: u32 = 2048;

/// A parabolic sine, `phase` in 0..65536 -> `-32768..=32768`.
fn sine16(phase: u32) -> i32 {
    let half = phase & 0x7FFF;
    let v = ((half * (32768 - half)) >> 13) as i32;
    if phase & 0x8000 == 0 {
        v
    } else {
        -v
    }
}

fn run() -> Result<(), i64> {
    let mut args = env::args().skip(1);
    let mut num = |d: u64| args.next().and_then(|s| s.parse().ok()).unwrap_or(d);
    let (ahead, ms, level) = (num(512), num(2000), num(10).min(100));
    let svc = ns::wait_for_timeout("audio", 3_000_000_000)?;
    let len = audio::RING_HEADER + FRAMES * 4;
    let shm = SharedMemory::new(len, 0)?;
    ipc::set_send_handle(0, shm.handle());
    let open = Msg::from_words(audio::OPEN, &[len as u64, ahead]).with_handles(1);
    let granted = ipc::call_checked(svc, &open)?;
    println!(
        "audiotest: ring of {} frames, {} periods of {} frames ahead",
        granted.mr[0], granted.mr[2], granted.mr[1]
    );
    let word = |at: usize| unsafe { &*(shm.as_ptr().add(at) as *const AtomicU32) };
    let data = unsafe { shm.as_ptr().add(audio::RING_HEADER) as *mut u32 };
    let (mut wpos, mut left, mut phase) = (0u32, ms * audio::RATE as u64 / 1000, 0u32);
    let step = ((440u64 << 16) / audio::RATE as u64) as u32;
    let amp = level as i32 * 300;
    // After the last frame, up to 400 ms for the service to take the rest.
    let mut drain = 0;
    while left > 0 || (drain < 100 && word(audio::RING_READ).load(Ordering::Acquire) != wpos) {
        let queued = wpos.wrapping_sub(word(audio::RING_READ).load(Ordering::Acquire));
        let n = (KEEP.saturating_sub(queued) as u64).min(left) as u32;
        for i in 0..n {
            let v = (sine16(phase) * amp / 32768) as i16 as u16 as u32;
            phase = phase.wrapping_add(step);
            let at = (wpos.wrapping_add(i) as usize) % FRAMES;
            unsafe { core::ptr::write_volatile(data.add(at), v | v << 16) };
        }
        wpos = wpos.wrapping_add(n);
        word(audio::RING_WRITE).store(wpos, Ordering::Release);
        left -= n as u64;
        if left == 0 {
            drain += 1;
        }
        time::sleep_ms(4);
    }
    println!(
        "audiotest: done, {} underruns",
        word(audio::RING_UNDERRUNS).load(Ordering::Relaxed)
    );
    let _ = ipc::call(svc, &Msg::new(audio::CLOSE, 0, 0));
    let _ = sys::handle_close(svc);
    Ok(())
}

fn main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            println!("audiotest: {}", err::name(e));
            1
        }
    }
}
