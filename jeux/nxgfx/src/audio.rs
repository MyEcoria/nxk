//! Sound: 16-bit stereo PCM at 48 kHz through the `audio` service. A
//! game makes `Sound`s (samples of its own, or the synthesised ones here),
//! `play`s them, and calls `pump` once a frame: it mixes the voices ahead
//! into a ring shared with the service, which plays silence when it runs
//! dry. A `Stream` hands the ring to a program that produces its own PCM
//! as it goes (a decoder).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::proto::audio as proto;
use nxk_abi::user::Msg;

use crate::os::{Result, Service, Shm};

pub const RATE: u32 = proto::RATE;
/// Frames in the shared ring (about 170 ms).
const RING_FRAMES: usize = 8192;
/// Frames kept queued ahead (about 43 ms).
const AHEAD: u32 = 2048;

/// Stereo PCM, two samples a frame, left then right.
#[derive(Clone)]
pub struct Sound {
    pcm: Arc<[i16]>,
}

impl Sound {
    pub fn from_stereo(pcm: Vec<i16>) -> Self {
        Self { pcm: pcm.into() }
    }

    pub fn from_mono(pcm: &[i16]) -> Self {
        Self::from_stereo(pcm.iter().flat_map(|&s| [s, s]).collect())
    }

    pub fn frames(&self) -> usize {
        self.pcm.len() / 2
    }

    /// A square-ish beep of `hz` for `ms`, starting at `level` (0..1) and
    /// fading to nothing if `fade`, with an optional glide to `hz_end`.
    pub fn tone(hz: f32, hz_end: f32, ms: u32, level: f32, fade: bool) -> Self {
        let n = ms as usize * RATE as usize / 1000;
        let mut out = Vec::with_capacity(n * 2);
        let mut phase = 0.0f32;
        for i in 0..n {
            let t = i as f32 / n as f32;
            let f = hz + (hz_end - hz) * t;
            phase += f / RATE as f32;
            phase -= (phase as i32) as f32;
            // Triangle wave: soft enough for a speaker this small.
            let tri = 1.0 - 4.0 * (phase - 0.5).abs();
            let env = if fade { 1.0 - t } else { 1.0 };
            let s = (tri * env * level * 30000.0) as i16;
            out.push(s);
            out.push(s);
        }
        Self::from_stereo(out)
    }

    /// Noise for `ms`, fading out: an explosion.
    pub fn noise(ms: u32, level: f32) -> Self {
        let n = ms as usize * RATE as usize / 1000;
        let mut out = Vec::with_capacity(n * 2);
        let (mut x, mut lp) = (0x1234_5678u32, 0.0f32);
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let white = (x >> 8) as f32 / (1 << 23) as f32 - 1.0;
            // A one-pole low-pass that opens as the sound dies away.
            let t = i as f32 / n as f32;
            lp += (white - lp) * (0.5 - 0.4 * t);
            let s = (lp * (1.0 - t) * level * 30000.0) as i16;
            out.push(s);
            out.push(s);
        }
        Self::from_stereo(out)
    }
}

/// A ring of frames shared with the service, and the frames written to it.
struct Ring {
    svc: Service,
    shm: Shm,
    frames: usize,
    wpos: u32,
    open: bool,
    /// The service's DMA period and the periods it keeps ahead (frames
    /// and count; 0 from a service that does not tell).
    period: u32,
    depth: u32,
}

impl Ring {
    fn open(frames: usize, ahead: u32) -> Result<Self> {
        let svc = Service::lookup("audio")?;
        let len = proto::RING_HEADER + frames * 4;
        let shm = Shm::new(len, 0)?;
        let r = svc.call_with(
            Msg::from_words(proto::OPEN, &[len as u64, ahead as u64]),
            shm.handle(),
        )?;
        let granted = |i: usize| if r.words() > i { r.mr[i] as u32 } else { 0 };
        Ok(Self {
            svc,
            shm,
            frames,
            wpos: 0,
            open: true,
            period: granted(1),
            depth: granted(2),
        })
    }

    fn word(&self, at: usize) -> &AtomicU32 {
        unsafe { &*(self.shm.as_ptr().add(at) as *const AtomicU32) }
    }

    /// Frames written and not yet taken by the service.
    fn queued(&self) -> u32 {
        let read = self.word(proto::RING_READ).load(Ordering::Acquire);
        self.wpos.wrapping_sub(read).min(self.frames as u32)
    }

    /// Frame `i` past the last one committed.
    fn put(&mut self, i: u32, left: i16, right: i16) {
        let data = unsafe { self.shm.as_ptr().add(proto::RING_HEADER) as *mut u32 };
        let w = (left as u16 as u32) | (right as u16 as u32) << 16;
        let at = (self.wpos.wrapping_add(i) as usize) % self.frames;
        unsafe { core::ptr::write_volatile(data.add(at), w) };
    }

    /// Hands the next `n` frames put to the service.
    fn commit(&mut self, n: u32) {
        self.wpos = self.wpos.wrapping_add(n);
        self.word(proto::RING_WRITE)
            .store(self.wpos, Ordering::Release);
    }

    fn close(&mut self) {
        if self.open {
            self.open = false;
            let _ = self.svc.call(&Msg::new(proto::CLOSE, 0, 0));
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.close();
    }
}

/// PCM a program writes as it makes it: the ring is played as it is
/// filled, silence when it runs dry.
pub struct Stream {
    ring: Ring,
}

impl Stream {
    /// A ring of `frames` frames with the audio service. Errors: no
    /// service (the emulator), or the codec did not come up.
    pub fn open(frames: usize) -> Result<Self> {
        Self::open_ahead(frames, 0)
    }

    /// Like `open`, with the service holding about `ahead` frames at most
    /// between the ring and the speakers (the default is 4096, 85 ms): for
    /// a program whose sound should follow what it writes closely. A
    /// service that does not know the request keeps its default.
    pub fn open_ahead(frames: usize, ahead: u32) -> Result<Self> {
        Ok(Self {
            ring: Ring::open(frames, ahead)?,
        })
    }

    /// The service's DMA period in frames and how many it keeps ahead of
    /// the speakers, as it granted them (0, 0 if it did not tell).
    pub fn dma(&self) -> (u32, u32) {
        (self.ring.period, self.ring.depth)
    }

    /// Frames waiting in the ring: the latency ahead of the speakers,
    /// beyond what the service holds in its DMA ring (`dma`).
    pub fn queued(&self) -> u32 {
        self.ring.queued()
    }

    pub fn capacity(&self) -> usize {
        self.ring.frames
    }

    /// Times the service found the ring empty.
    pub fn underruns(&self) -> u32 {
        self.ring
            .word(proto::RING_UNDERRUNS)
            .load(Ordering::Relaxed)
    }

    /// Writes interleaved stereo `pcm` (left then right) as far as there
    /// is room; returns the frames written.
    pub fn write(&mut self, pcm: &[i16]) -> usize {
        let room = self.ring.frames - self.ring.queued() as usize;
        let n = (pcm.len() / 2).min(room);
        for i in 0..n {
            self.ring.put(i as u32, pcm[2 * i], pcm[2 * i + 1]);
        }
        self.ring.commit(n as u32);
        n
    }

    pub fn close(&mut self) {
        self.ring.close();
    }
}

struct Voice {
    id: u32,
    sound: Sound,
    pos: usize,
    gain: i32,
    looped: bool,
}

pub struct Audio {
    ring: Ring,
    voices: Vec<Voice>,
    next_id: u32,
}

impl Audio {
    /// Share a ring with the audio service (which powers the codec up
    /// on the first request). Errors: no service (the emulator), or
    /// the codec did not come up.
    pub fn open() -> Result<Self> {
        Ok(Self {
            ring: Ring::open(RING_FRAMES, 0)?,
            voices: Vec::new(),
            next_id: 1,
        })
    }

    /// Start `sound` at `volume` (0..1); the voice's id.
    pub fn play(&mut self, sound: &Sound, volume: f32) -> u32 {
        self.start(sound, volume, false)
    }

    /// Like `play`, again and again until `stop`.
    pub fn play_looped(&mut self, sound: &Sound, volume: f32) -> u32 {
        self.start(sound, volume, true)
    }

    fn start(&mut self, sound: &Sound, volume: f32, looped: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        if sound.frames() > 0 && self.voices.len() < 16 {
            self.voices.push(Voice {
                id,
                sound: sound.clone(),
                pos: 0,
                gain: (volume.clamp(0.0, 1.0) * 256.0) as i32,
                looped,
            });
        }
        id
    }

    pub fn stop(&mut self, id: u32) {
        self.voices.retain(|v| v.id != id);
    }

    pub fn stop_all(&mut self) {
        self.voices.clear();
    }

    /// Mix ahead into the ring: once a frame.
    pub fn pump(&mut self) {
        if self.voices.is_empty() {
            return;
        }
        let n = AHEAD.saturating_sub(self.ring.queued());
        for i in 0..n {
            let (mut l, mut r) = (0i32, 0i32);
            for v in self.voices.iter_mut() {
                if v.pos >= v.sound.pcm.len() {
                    continue;
                }
                l += (v.sound.pcm[v.pos] as i32 * v.gain) >> 8;
                r += (v.sound.pcm[v.pos + 1] as i32 * v.gain) >> 8;
                v.pos += 2;
                if v.looped && v.pos >= v.sound.pcm.len() {
                    v.pos = 0;
                }
            }
            self.ring.put(
                i,
                l.clamp(-32768, 32767) as i16,
                r.clamp(-32768, 32767) as i16,
            );
        }
        self.ring.commit(n);
        self.voices
            .retain(|v| v.looped || v.pos < v.sound.pcm.len());
    }

    pub fn close(&mut self) {
        self.ring.close();
    }
}
