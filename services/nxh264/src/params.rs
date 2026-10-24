//! Sequence and picture parameter sets, and the slice header: what the
//! stream of the Luna server uses (High profile, CABAC, progressive 4:2:0,
//! 8 bits, flat scaling), the rest refused with `Error::Unsupported`.

use crate::bits::{Bits, Eof};
use crate::Error;

impl From<Eof> for Error {
    fn from(_: Eof) -> Self {
        Error::Truncated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub num_ref_frames: u32,
    pub mb_width: u32,
    pub mb_height: u32,
    /// Cropping in luma samples: left, right, top, bottom.
    pub crop: [u32; 4],
}

impl Sps {
    pub fn width(&self) -> u32 {
        self.mb_width * 16 - self.crop[0] - self.crop[1]
    }

    pub fn height(&self) -> u32 {
        self.mb_height * 16 - self.crop[2] - self.crop[3]
    }

    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
}

fn skip_scaling_list(b: &mut Bits, size: usize) -> Result<(), Error> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + b.se()? + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

/// Parses a sequence parameter set (RBSP after the NAL header byte).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps, Error> {
    let mut b = Bits::new(rbsp);
    let profile_idc = b.u(8)? as u8;
    b.u(8)?; // constraint flags and reserved bits
    let level_idc = b.u(8)? as u8;
    let id = b.ue()?;
    if id > 31 {
        return Err(Error::Invalid("sps id"));
    }
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if b.ue()? != 1 {
            return Err(Error::Unsupported("chroma format other than 4:2:0"));
        }
        if b.ue()? != 0 || b.ue()? != 0 {
            return Err(Error::Unsupported("bit depth above 8"));
        }
        if b.flag()? {
            return Err(Error::Unsupported("lossless transform bypass"));
        }
        if b.flag()? {
            // The lists are read to keep the stream in step, then refused
            // unless they are all "use the default or flat".
            for i in 0..8 {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                    return Err(Error::Unsupported("scaling matrices"));
                }
            }
            return Err(Error::Unsupported("scaling matrices"));
        }
    }
    let log2_max_frame_num = b.ue()? + 4;
    let poc_type = b.ue()?;
    let mut log2_max_poc_lsb = 0;
    match poc_type {
        0 => log2_max_poc_lsb = b.ue()? + 4,
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        2 => {}
        _ => return Err(Error::Invalid("poc type")),
    }
    let num_ref_frames = b.ue()?;
    b.bit()?; // gaps_in_frame_num_value_allowed_flag
    let mb_width = b.ue()? + 1;
    let map_height = b.ue()? + 1;
    if !b.flag()? {
        return Err(Error::Unsupported("interlaced video"));
    }
    b.bit()?; // direct_8x8_inference_flag
    let mut crop = [0; 4];
    if b.flag()? {
        // Units of two samples in 4:2:0 (frame_mbs_only).
        for c in crop.iter_mut() {
            *c = b.ue()? * 2;
        }
    }
    if mb_width > 512 || map_height > 512 || log2_max_frame_num > 16 || num_ref_frames > 16 {
        return Err(Error::Invalid("sps size"));
    }
    if crop[0] + crop[1] >= mb_width * 16 || crop[2] + crop[3] >= map_height * 16 {
        return Err(Error::Invalid("crop"));
    }
    Ok(Sps {
        id,
        profile_idc,
        level_idc,
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        num_ref_frames,
        mb_width,
        mb_height: map_height,
        crop,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub bottom_field_pic_order: bool,
    pub num_ref_idx_default: [u32; 2],
    pub pic_init_qp: i32,
    pub chroma_qp_offset: [i32; 2],
    pub deblocking_control: bool,
    pub constrained_intra_pred: bool,
    pub transform_8x8: bool,
}

/// Parses a picture parameter set (RBSP after the NAL header byte).
pub fn parse_pps(rbsp: &[u8]) -> Result<Pps, Error> {
    let mut b = Bits::new(rbsp);
    let id = b.ue()?;
    let sps_id = b.ue()?;
    if id > 255 || sps_id > 31 {
        return Err(Error::Invalid("pps id"));
    }
    if !b.flag()? {
        return Err(Error::Unsupported("CAVLC"));
    }
    let bottom_field_pic_order = b.flag()?;
    if b.ue()? != 0 {
        return Err(Error::Unsupported("slice groups"));
    }
    let l0 = b.ue()? + 1;
    let l1 = b.ue()? + 1;
    if l0 > 32 || l1 > 32 {
        return Err(Error::Invalid("num_ref_idx_default"));
    }
    if b.flag()? {
        return Err(Error::Unsupported("weighted prediction"));
    }
    if b.u(2)? != 0 {
        return Err(Error::Unsupported("weighted bi-prediction"));
    }
    let pic_init_qp = 26 + b.se()?;
    b.se()?; // pic_init_qs
    let cb = b.se()?;
    let deblocking_control = b.flag()?;
    let constrained_intra_pred = b.flag()?;
    if b.flag()? {
        return Err(Error::Unsupported("redundant pictures"));
    }
    let (mut transform_8x8, mut cr) = (false, cb);
    if b.more_rbsp_data() {
        transform_8x8 = b.flag()?;
        if b.flag()? {
            return Err(Error::Unsupported("scaling matrices"));
        }
        cr = b.se()?;
    }
    if !(-12..=12).contains(&cb) || !(-12..=12).contains(&cr) {
        return Err(Error::Invalid("chroma qp offset"));
    }
    Ok(Pps {
        id,
        sps_id,
        bottom_field_pic_order,
        num_ref_idx_default: [l0, l1],
        pic_init_qp,
        chroma_qp_offset: [cb, cr],
        deblocking_control,
        constrained_intra_pred,
        transform_8x8,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    I,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_ref_idc: u8,
    pub idr: bool,
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    /// Active reference indices (list 0).
    pub num_ref_idx: u32,
    pub cabac_init_idc: u32,
    pub qp: i32,
    pub disable_deblocking: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
    /// Bit position of the slice data (byte aligned for CABAC) in the RBSP.
    pub data_start: usize,
}

/// Parses a slice header. `nal_type` is 1 or 5.
pub fn parse_slice_header(
    rbsp: &[u8],
    nal_type: u8,
    nal_ref_idc: u8,
    sps_of: impl Fn(u32) -> Option<(Sps, Pps)>,
) -> Result<(SliceHeader, Sps, Pps), Error> {
    let mut b = Bits::new(rbsp);
    let idr = nal_type == 5;
    let first_mb = b.ue()?;
    let st = b.ue()?;
    let slice_type = match st % 5 {
        0 => SliceType::P,
        2 => SliceType::I,
        1 => return Err(Error::Unsupported("B slices")),
        _ => return Err(Error::Unsupported("SP or SI slices")),
    };
    let pps_id = b.ue()?;
    let (sps, pps) = sps_of(pps_id).ok_or(Error::Invalid("slice names a missing parameter set"))?;
    let frame_num = b.u(sps.log2_max_frame_num)?;
    let idr_pic_id = if idr { b.ue()? } else { 0 };
    match sps.poc_type {
        0 => {
            b.u(sps.log2_max_poc_lsb)?;
            if pps.bottom_field_pic_order {
                b.se()?;
            }
        }
        1 => return Err(Error::Unsupported("poc type 1")),
        _ => {}
    }
    let mut num_ref_idx = pps.num_ref_idx_default[0];
    if slice_type == SliceType::P {
        if b.flag()? {
            num_ref_idx = b.ue()? + 1;
            if num_ref_idx > 32 {
                return Err(Error::Invalid("num_ref_idx_active"));
            }
        }
        // ref_pic_list_modification_flag_l0
        if b.flag()? {
            return Err(Error::Unsupported("reference list modification"));
        }
    }
    if nal_ref_idc != 0 {
        if idr {
            b.bit()?; // no_output_of_prior_pics_flag
            if b.flag()? {
                return Err(Error::Unsupported("long-term references"));
            }
        } else if b.flag()? {
            return Err(Error::Unsupported("memory management control operations"));
        }
    }
    let cabac_init_idc = if slice_type == SliceType::P {
        let v = b.ue()?;
        if v > 2 {
            return Err(Error::Invalid("cabac_init_idc"));
        }
        v
    } else {
        0
    };
    let qp = pps.pic_init_qp + b.se()?;
    if !(0..=51).contains(&qp) {
        return Err(Error::Invalid("slice qp"));
    }
    let (mut disable_deblocking, mut alpha_offset, mut beta_offset) = (0, 0, 0);
    if pps.deblocking_control {
        disable_deblocking = b.ue()?;
        if disable_deblocking > 2 {
            return Err(Error::Invalid("disable_deblocking_filter_idc"));
        }
        if disable_deblocking != 1 {
            alpha_offset = b.se()? * 2;
            beta_offset = b.se()? * 2;
            if !(-12..=12).contains(&alpha_offset) || !(-12..=12).contains(&beta_offset) {
                return Err(Error::Invalid("deblocking offsets"));
            }
        }
    }
    // cabac_alignment_one_bit up to the next byte.
    b.align();
    let header = SliceHeader {
        nal_ref_idc,
        idr,
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        idr_pic_id,
        num_ref_idx,
        cabac_init_idc,
        qp,
        disable_deblocking,
        alpha_offset,
        beta_offset,
        data_start: b.pos() / 8,
    };
    Ok((header, sps, pps))
}
