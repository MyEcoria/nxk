//! The only CELT mode Opus uses: 48 kHz, 2.5 ms short blocks (120
//! coefficients), up to 8 of them per frame (LM 3, 20 ms), 21 bands.

pub const NB_EBANDS: usize = 21;
pub const SHORT_MDCT_SIZE: usize = 120;
pub const OVERLAP: usize = 120;
pub const MAX_LM: i32 = 3;
pub const NB_ALLOC_VECTORS: usize = 11;
/// The de-emphasis coefficient (`mode->preemph[0]`).
pub const PREEMPH: f32 = 0.850_006_1;
