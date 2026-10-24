//! An H.264 decoder for nxk: the stream Luna's server sends (High profile,
//! CABAC, 8x8 transform, progressive 4:2:0, 8 bits, I and P slices).
//!
//! `Decoder::decode` takes one access unit in Annex B form (a picture, its
//! slices as NAL units) and gives the picture back. Slices of a picture
//! decode one after the other; the deblocking filter runs when the last
//! one is in.

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
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            sps: vec![None; 32],
            pps: vec![None; 256],
            deblock: true,
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
        let mut cur: Option<Picture> = None;
        let mut cur_hdr = None;
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
                    if cur.is_none() {
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
                        let mut p = self.take_picture(&sps);
                        p.crop = sps.crop;
                        p.chroma_qp_offset = pps.chroma_qp_offset;
                        p.frame_num = hdr.frame_num;
                        self.next_id += 1;
                        p.id = self.next_id;
                        cur = Some(p);
                    }
                    let pic = cur.as_mut().expect("picture");
                    pic.slices.push(SliceFilter {
                        disable: hdr.disable_deblocking,
                        alpha_offset: hdr.alpha_offset,
                        beta_offset: hdr.beta_offset,
                    });
                    let refs: Vec<&Picture> = if hdr.slice_type == SliceType::P {
                        self.dpb.iter().map(|a| &**a).collect()
                    } else {
                        Vec::new()
                    };
                    let inp = mb::SliceInput {
                        sps: &sps,
                        pps: &pps,
                        hdr: &hdr,
                        refs: &refs,
                        slice_id: pic.slices.len() as u16,
                    };
                    mb::decode_slice(&inp, &rbsp, pic)?;
                    cur_hdr = Some((hdr, sps));
                }
                _ => {}
            }
        }
        let (Some(mut pic), Some((hdr, sps))) = (cur, cur_hdr) else {
            return Ok(None);
        };
        if self.deblock {
            deblock::deblock_picture(&mut pic);
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
