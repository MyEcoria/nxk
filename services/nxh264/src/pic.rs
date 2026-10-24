//! Decoded pictures: the three planes and what the decoder keeps of each
//! macroblock (for prediction by its neighbours, the deblocking filter and
//! as the motion source of later pictures).

#[derive(Clone)]
pub struct Plane {
    pub data: Vec<u8>,
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl Plane {
    fn new(width: usize, height: usize) -> Self {
        Self {
            data: vec![0; width * height],
            stride: width,
            width,
            height,
        }
    }
}

/// How a macroblock was coded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    /// Not decoded (yet).
    #[default]
    None,
    I4x4,
    I8x8,
    I16x16,
    Pcm,
    /// Inter, with residual or not.
    Inter,
    /// P_Skip.
    Skip,
}

impl Kind {
    pub fn is_intra(self) -> bool {
        matches!(self, Kind::I4x4 | Kind::I8x8 | Kind::I16x16 | Kind::Pcm)
    }
}

#[derive(Clone, Copy)]
pub struct MbInfo {
    /// 1-based number of the slice in the picture; 0 while not decoded.
    pub slice: u16,
    pub kind: Kind,
    pub qp: i8,
    /// Luma coded block pattern in bits 0..4, chroma in bits 4..6.
    pub cbp: u8,
    pub transform_8x8: bool,
    pub chroma_mode: u8,
    /// Intra 4x4 / 8x8 prediction modes, raster order of the 4x4 blocks.
    pub modes: [i8; 16],
    /// coded_block_flag of the luma 4x4 blocks (raster), of the Cb and Cr
    /// AC blocks, and of the DC blocks (bit 0 luma, 1 Cb, 2 Cr).
    pub cbf_luma: u16,
    pub cbf_cb: u8,
    pub cbf_cr: u8,
    pub cbf_dc: u8,
    /// Motion vectors in quarter samples, raster order of the 4x4 blocks.
    pub mv: [[i16; 2]; 16],
    /// Absolute motion vector differences, saturated (CABAC contexts).
    pub mvd: [[u8; 2]; 16],
    /// Reference index per 8x8 block (-1: none) and the picture it names.
    pub ref_idx: [i8; 4],
    pub ref_pic: [u32; 4],
}

impl Default for MbInfo {
    fn default() -> Self {
        Self {
            slice: 0,
            kind: Kind::None,
            qp: 0,
            cbp: 0,
            transform_8x8: false,
            chroma_mode: 0,
            modes: [2; 16],
            cbf_luma: 0,
            cbf_cb: 0,
            cbf_cr: 0,
            cbf_dc: 0,
            mv: [[0; 2]; 16],
            mvd: [[0; 2]; 16],
            ref_idx: [-1; 4],
            ref_pic: [u32::MAX; 4],
        }
    }
}

/// What the deblocking filter needs to know of a slice.
#[derive(Clone, Copy, Default)]
pub struct SliceFilter {
    pub disable: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
}

#[derive(Clone)]
pub struct Picture {
    pub mb_width: usize,
    pub mb_height: usize,
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
    pub mbs: Vec<MbInfo>,
    pub slices: Vec<SliceFilter>,
    pub chroma_qp_offset: [i32; 2],
    /// Unique number of the picture, to tell references apart.
    pub id: u32,
    pub frame_num: u32,
    /// Cropping of the displayed picture: left, right, top, bottom.
    pub crop: [u32; 4],
}

impl Picture {
    pub fn new(mb_width: usize, mb_height: usize) -> Self {
        let (w, h) = (mb_width * 16, mb_height * 16);
        Self {
            mb_width,
            mb_height,
            y: Plane::new(w, h),
            u: Plane::new(w / 2, h / 2),
            v: Plane::new(w / 2, h / 2),
            mbs: vec![MbInfo::default(); mb_width * mb_height],
            slices: Vec::new(),
            chroma_qp_offset: [0; 2],
            id: 0,
            frame_num: 0,
            crop: [0; 4],
        }
    }

    /// Forgets the macroblocks of the last picture, for a new one.
    pub fn reset(&mut self) {
        self.mbs.fill(MbInfo::default());
        self.slices.clear();
    }

    pub fn width(&self) -> usize {
        self.mb_width * 16 - (self.crop[0] + self.crop[1]) as usize
    }

    pub fn height(&self) -> usize {
        self.mb_height * 16 - (self.crop[2] + self.crop[3]) as usize
    }

    /// The cropped picture as I420 bytes (Y, then U, then V).
    pub fn to_i420(&self) -> Vec<u8> {
        let (w, h) = (self.width(), self.height());
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        let (x0, y0) = (self.crop[0] as usize, self.crop[2] as usize);
        for row in 0..h {
            let o = (y0 + row) * self.y.stride + x0;
            out.extend_from_slice(&self.y.data[o..o + w]);
        }
        for plane in [&self.u, &self.v] {
            for row in 0..h / 2 {
                let o = (y0 / 2 + row) * plane.stride + x0 / 2;
                out.extend_from_slice(&plane.data[o..o + w / 2]);
            }
        }
        out
    }
}
