//! An H.264 decoder for nxk: the stream Luna's server sends (High profile,
//! CABAC, 8x8 transform, progressive 4:2:0, 8 bits, I and P slices).
//!
//! `Decoder::decode` takes one access unit in Annex B form (a picture, its
//! slices as NAL units) and gives the picture back. Slices of a picture
//! decode one after the other; the deblocking filter runs when the last
//! one is in.

// The loops index blocks and samples the way the standard's formulas do, and
// the prediction functions take the plane, the position and the neighbours.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

pub mod bits;
pub mod cabac;
pub mod deblock;
pub mod inter;
pub mod intra;
pub mod mb;
pub mod params;
pub mod pic;
pub mod tables;
pub mod transform;

use std::sync::Arc;

pub use pic::Picture;

use params::{parse_pps, parse_slice_header, parse_sps, Pps, SliceType, Sps};
use pic::SliceFilter;

/// Why a stream could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The data ended inside a syntax element.
    Truncated,
    /// A value the standard does not allow.
    Invalid(&'static str),
    /// A feature this decoder does not have.
    Unsupported(&'static str),
    /// A reference picture is missing (a frame was lost): wait for a key frame.
    Gap,
}

/// The NAL units of an Annex B stream, without their start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = if k + 1 < starts.len() {
            starts[k + 1] - 3
        } else {
            stream.len()
        };
        while end > s && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&stream[s..end]);
        }
    }
    out
}

#[derive(Default)]
pub struct Decoder {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    /// Reference pictures, the newest first.
    dpb: Vec<Arc<Picture>>,
    spare: Vec<Arc<Picture>>,
    next_id: u32,
    /// frame_num of the last reference picture, if the stream can go on from it.
    last_ref_frame_num: Option<u32>,
    /// The deblocking filter, on unless a test turns it off.
    pub deblock: bool,
    /// Threads to decode the slices of a picture with (1: this one).
    pub threads: usize,
    /// Time spent in slices and in the deblocking filter, nanoseconds.
    pub slice_ns: u64,
    pub deblock_ns: u64,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            sps: vec![None; 32],
            pps: vec![None; 256],
            deblock: true,
            threads: 1,
            ..Self::default()
        }
    }

    /// A picture to decode into, reused when one is free.
    fn take_picture(&mut self, sps: &Sps) -> Picture {
        let (w, h) = (sps.mb_width as usize, sps.mb_height as usize);
        let mut found = None;
        for (i, a) in self.spare.iter().enumerate() {
            if Arc::strong_count(a) == 1 && a.mb_width == w && a.mb_height == h {
                found = Some(i);
                break;
            }
        }
        let mut pic = match found {
            Some(i) => Arc::try_unwrap(self.spare.swap_remove(i)).unwrap_or_else(|a| (*a).clone()),
            None => Picture::new(w, h),
        };
        pic.reset();
        pic
    }

    /// Decodes one access unit. `Ok(None)` when it holds no picture.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<Arc<Picture>>, Error> {
        struct Job {
            hdr: params::SliceHeader,
            rbsp: Vec<u8>,
        }
        let mut jobs: Vec<Job> = Vec::new();
        let mut first = None;
        for nal in nal_units(au) {
            let (ty, ref_idc) = (nal[0] & 31, (nal[0] >> 5) & 3);
            let rbsp = bits::unescape(&nal[1..]);
            match ty {
                7 => {
                    let s = parse_sps(&rbsp)?;
                    let id = s.id as usize;
                    self.sps[id] = Some(s);
                }
                8 => {
                    let p = parse_pps(&rbsp)?;
                    let id = p.id as usize;
                    self.pps[id] = Some(p);
                }
                1 | 5 => {
                    let lookup = |id: u32| {
                        let p = self.pps.get(id as usize)?.clone()?;
                        let s = self.sps.get(p.sps_id as usize)?.clone()?;
                        Some((s, p))
                    };
                    let (hdr, sps, pps) = parse_slice_header(&rbsp, ty, ref_idc, lookup)?;
                    if first.is_none() {
                        first = Some((hdr.clone(), sps, pps));
                    }
                    jobs.push(Job { hdr, rbsp });
                }
                _ => {}
            }
        }
        let Some((hdr, sps, pps)) = first else {
            return Ok(None);
        };
        // Picture level: key frame, lost references.
        if hdr.idr {
            self.dpb.clear();
            self.last_ref_frame_num = Some(hdr.frame_num);
        } else {
            if self.dpb.is_empty() {
                return Err(Error::Gap);
            }
            let expect = self
                .last_ref_frame_num
                .map(|n| (n + 1) % sps.max_frame_num());
            if expect.is_some_and(|e| e != hdr.frame_num)
                && self.last_ref_frame_num != Some(hdr.frame_num)
            {
                self.dpb.clear();
                self.last_ref_frame_num = None;
                return Err(Error::Gap);
            }
        }
        let mut pic = self.take_picture(&sps);
        pic.crop = sps.crop;
        pic.chroma_qp_offset = pps.chroma_qp_offset;
        pic.frame_num = hdr.frame_num;
        self.next_id += 1;
        pic.id = self.next_id;

        // Which macroblocks each slice has: from its first to the next slice's first.
        let total = pic.mbs.len();
        let mb_w = pic.mb_width;
        let mut slice_map = vec![0u16; total];
        let mut prev_start = 0usize;
        for (i, j) in jobs.iter().enumerate() {
            let start = j.hdr.first_mb as usize;
            if start >= total || (i > 0 && start <= prev_start) {
                return Err(Error::Invalid("slice order"));
            }
            let end = jobs
                .get(i + 1)
                .map_or(total, |n| n.hdr.first_mb as usize)
                .min(total);
            if end <= start {
                return Err(Error::Invalid("slice order"));
            }
            slice_map[start..end].fill(i as u16 + 1);
            prev_start = start;
            pic.slices.push(SliceFilter {
                disable: j.hdr.disable_deblocking,
                alpha_offset: j.hdr.alpha_offset,
                beta_offset: j.hdr.beta_offset,
            });
        }
        // Bands: one per slice when every slice starts on a row, else the whole picture.
        let aligned = jobs
            .iter()
            .all(|j| (j.hdr.first_mb as usize).is_multiple_of(mb_w));
        let cuts: Vec<usize> = if aligned {
            jobs.iter()
                .skip(1)
                .map(|j| j.hdr.first_mb as usize / mb_w)
                .collect()
        } else {
            Vec::new()
        };
        let band_jobs: Vec<Vec<usize>> = if aligned {
            (0..jobs.len()).map(|i| vec![i]).collect()
        } else {
            vec![(0..jobs.len()).collect()]
        };
        let refs: Vec<&Picture> = if jobs.iter().any(|j| j.hdr.slice_type == SliceType::P) {
            self.dpb.iter().map(|a| &**a).collect()
        } else {
            Vec::new()
        };
        let cqo = pic.chroma_qp_offset;
        let t = std::time::Instant::now();
        let result = {
            let bands: Vec<std::sync::Mutex<Option<pic::Band>>> = pic
                .bands(&cuts)
                .into_iter()
                .map(|b| std::sync::Mutex::new(Some(b)))
                .collect();
            let next = std::sync::atomic::AtomicUsize::new(0);
            let error: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);
            let (sps, pps, slice_map, refs, jobs, band_jobs) =
                (&sps, &pps, &slice_map, &refs, &jobs, &band_jobs);
            let work = || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= bands.len() {
                    break;
                }
                let mut band = bands[i].lock().unwrap().take().expect("band taken once");
                for &k in &band_jobs[i] {
                    let job = &jobs[k];
                    let inp = mb::SliceInput {
                        sps,
                        pps,
                        hdr: &job.hdr,
                        refs,
                        slice_id: k as u16 + 1,
                    };
                    if let Err(e) = mb::decode_slice(&inp, &job.rbsp, &mut band, slice_map, cqo) {
                        error.lock().unwrap().get_or_insert(e);
                        break;
                    }
                }
            };
            let workers = self.threads.clamp(1, bands.len());
            if workers == 1 {
                work();
            } else {
                std::thread::scope(|sc| {
                    for _ in 1..workers {
                        sc.spawn(work);
                    }
                    work();
                });
            }
            error.into_inner().unwrap()
        };
        self.slice_ns += t.elapsed().as_nanos() as u64;
        if let Some(e) = result {
            return Err(e);
        }
        if self.deblock {
            let t = std::time::Instant::now();
            deblock::deblock_picture(&mut pic, self.threads);
            self.deblock_ns += t.elapsed().as_nanos() as u64;
        }
        let pic = Arc::new(pic);
        if hdr.nal_ref_idc != 0 {
            if self.dpb.len() >= sps.num_ref_frames.max(1) as usize {
                if let Some(old) = self.dpb.pop() {
                    self.spare.push(old);
                }
            }
            self.dpb.insert(0, pic.clone());
            self.last_ref_frame_num = Some(hdr.frame_num);
        }
        self.spare.push(pic.clone());
        self.spare
            .retain(|a| a.mb_width == pic.mb_width && a.mb_height == pic.mb_height);
        while self.spare.len() > 6 {
            self.spare.remove(0);
        }
        Ok(Some(pic))
    }
}
