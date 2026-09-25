//! GM20B firmware containers (nouveau include/nvfw/*.h) and the WPR
//! image the ACR loader copies into the GPU firmware carveout, with FECS
//! booted by the loader itself (non-lazy, NVIDIA's gm20b flow). Pure,
//! tested on the host (against the real files when `firmware/` has them):
//! `rustc --edition 2021 --test user/gpu/src/fw.rs -o fw && ./fw`.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

pub type Result<T> = core::result::Result<T, &'static str>;

fn u32_at(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .ok_or("truncated")
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn slice(b: &[u8], off: u32, len: u32) -> Result<&[u8]> {
    b.get(off as usize..(off as usize).checked_add(len as usize).ok_or("bad size")?)
        .ok_or("truncated")
}

fn align(v: u32, a: u32) -> u32 {
    v.next_multiple_of(a)
}

/// `nvfw_bin_hdr`: magic 0x3b1d14f0 (NVIDIA's releases for nouveau).
struct BinHdr {
    header_offset: u32,
    data_offset: u32,
    data_size: u32,
}

const BIN_MAGIC: u32 = 0x3B1D_14F0;

fn bin_hdr(b: &[u8]) -> Result<BinHdr> {
    if u32_at(b, 0)? != BIN_MAGIC {
        return Err("not an NVIDIA firmware container");
    }
    Ok(BinHdr {
        header_offset: u32_at(b, 12)?,
        data_offset: u32_at(b, 16)?,
        data_size: u32_at(b, 20)?,
    })
}

/// A falcon bootloader (`nvfw_bl_desc`): code and its IMEM tag address.
pub struct Bl {
    pub code: Vec<u8>,
    /// start_tag << 8: the virtual IMEM address it runs at.
    pub boot_addr: u32,
}

pub fn bl(b: &[u8]) -> Result<Bl> {
    let h = bin_hdr(b)?;
    let d = h.header_offset as usize;
    let (tag, code_off, code_size) = (u32_at(b, d)?, u32_at(b, d + 8)?, u32_at(b, d + 12)?);
    Ok(Bl {
        code: slice(b, h.data_offset + code_off, code_size)?.to_vec(),
        boot_addr: tag << 8,
    })
}

/// The ACR "load" HS ucode (`nvkm_falcon_fw_ctor_hs`): one DMA image,
/// non-secure code, secure code and data, plus its two signatures.
pub struct Hs {
    pub img: Vec<u8>,
    pub nmem_base: u32,
    pub nmem_size: u32,
    pub imem_base: u32,
    pub imem_size: u32,
    pub dmem_base_img: u32,
    pub dmem_size: u32,
    sig_loc: u32,
    sig_prod: Vec<u8>,
    sig_dbg: Vec<u8>,
}

pub fn hs(b: &[u8]) -> Result<Hs> {
    let h = bin_hdr(b)?;
    let s = h.header_offset as usize;
    let sig_dbg_off = u32_at(b, s)?;
    let sig_prod_off = u32_at(b, s + 8)?;
    let sig_size = u32_at(b, s + 12)?;
    // 0x3b1d14f0 format: patch location and signature index given directly.
    let (loc, sig) = (u32_at(b, s + 16)?, u32_at(b, s + 20)?);
    let l = u32_at(b, s + 24)? as usize;
    let num_apps = u32_at(b, l + 16)? as usize;
    let app0 = u32_at(b, l + 20)?;
    let img = slice(b, h.data_offset, h.data_size)?.to_vec();
    let hs = Hs {
        nmem_base: u32_at(b, l)?,
        nmem_size: u32_at(b, l + 4)?,
        dmem_base_img: u32_at(b, l + 8)?,
        dmem_size: u32_at(b, l + 12)?,
        imem_base: align(app0, 0x100),
        imem_size: u32_at(b, l + 20 + 4 * num_apps)?,
        sig_loc: loc,
        sig_prod: slice(b, sig_prod_off + sig, sig_size)?.to_vec(),
        sig_dbg: slice(b, sig_dbg_off + sig, sig_size)?.to_vec(),
        img,
    };
    if app0 != hs.imem_base
        || hs.nmem_base != 0
        || (hs.dmem_base_img + hs.dmem_size) as usize > hs.img.len()
        || hs.sig_loc as usize + hs.sig_prod.len() > hs.img.len()
    {
        return Err("unexpected HS layout");
    }
    Ok(hs)
}

/// `flcn_acr_desc` fields in the HS data: ucode blob size and base.
const ACR_DESC_BLOB_SIZE: usize = 0x240;
const ACR_DESC_BLOB_BASE: usize = 0x248;

impl Hs {
    /// Production or debug signature, by the falcon's debug fuse
    /// (`gm200_flcn_fw_signature`).
    pub fn sign(&mut self, debug: bool) {
        let s = if debug { &self.sig_dbg } else { &self.sig_prod };
        let at = self.sig_loc as usize;
        self.img[at..at + s.len()].copy_from_slice(s);
    }

    /// Where the loader finds the WPR image to copy (`gm20b_acr_load_setup`).
    pub fn set_blob(&mut self, base: u64, size: u32) {
        let d = self.dmem_base_img as usize;
        put32(&mut self.img, d + ACR_DESC_BLOB_SIZE, size);
        put32(&mut self.img, d + ACR_DESC_BLOB_BASE, base as u32);
        put32(
            &mut self.img,
            d + ACR_DESC_BLOB_BASE + 4,
            (base >> 32) as u32,
        );
    }
}

/// `flcn_bl_dmem_desc` (v0): what a bootloader DMAs, from where.
pub struct BlDmem {
    pub ctx_dma: u32,
    /// Addresses in 256-byte units.
    pub code_dma_base: u64,
    pub non_sec_code_off: u32,
    pub non_sec_code_size: u32,
    pub sec_code_off: u32,
    pub sec_code_size: u32,
    pub code_entry_point: u32,
    pub data_dma_base: u64,
    pub data_size: u32,
}

pub const BL_DMEM_SIZE: usize = 76;

impl BlDmem {
    pub fn bytes(&self) -> [u8; BL_DMEM_SIZE] {
        let mut b = [0u8; BL_DMEM_SIZE];
        // reserved[4], signature[4] stay zero.
        let w = [
            self.ctx_dma,
            self.code_dma_base as u32,
            self.non_sec_code_off,
            self.non_sec_code_size,
            self.sec_code_off,
            self.sec_code_size,
            self.code_entry_point,
            self.data_dma_base as u32,
            self.data_size,
            (self.code_dma_base >> 32) as u32,
            (self.data_dma_base >> 32) as u32,
        ];
        for (i, v) in w.iter().enumerate() {
            put32(&mut b, 32 + 4 * i, *v);
        }
        b
    }
}

/// Falcon DMA indices (nvkm/engine/falcon.h).
pub const DMAIDX_UCODE: u32 = 0;
pub const DMAIDX_VIRT: u32 = 1;
/// LS falcon IDs (nvkm/subdev/acr.h).
pub const FALCON_FECS: u32 = 2;

/// An LS falcon image (`nvkm_acr_lsfw_load_bl_inst_data_sig`):
/// bootloader, then resident code, then resident data.
pub struct Ls {
    pub img: Vec<u8>,
    sig: Vec<u8>,
    bl_size: u32,
    bl_imem_off: u32,
    code_size: u32,
    data_size: u32,
}

/// `lsf_signature`: two keys each for production and debug, flags, ID.
const LS_SIG_SIZE: usize = 76;

pub fn ls(bl_file: &[u8], inst: &[u8], data: &[u8], sig: &[u8]) -> Result<Ls> {
    let h = bin_hdr(bl_file)?;
    let d = h.header_offset as usize;
    let (tag, code_size) = (u32_at(bl_file, d)?, u32_at(bl_file, d + 12)?);
    if sig.len() != LS_SIG_SIZE {
        return Err("bad LS signature size");
    }
    let bl_size = align(code_size, 256);
    let code = align(inst.len() as u32, 256);
    let dsize = align(data.len() as u32, 256);
    let mut img = vec![0u8; (bl_size + code + dsize) as usize];
    // nouveau copies the aligned size from the data section.
    let src = slice(bl_file, h.data_offset, bl_size.min(h.data_size))?;
    img[..src.len()].copy_from_slice(src);
    img[bl_size as usize..bl_size as usize + inst.len()].copy_from_slice(inst);
    let at = (bl_size + code) as usize;
    img[at..at + data.len()].copy_from_slice(data);
    Ok(Ls {
        img,
        sig: sig.to_vec(),
        bl_size,
        bl_imem_off: tag << 8,
        code_size: code,
        data_size: dsize,
    })
}

/// `wpr_header` (20 bytes) slots reserved at the start (MAX_LSF).
const WPR_HEADERS: u32 = 11;
const WPR_HEADER_SIZE: u32 = 20;
const WPR_STATUS_COPY: u32 = 1;
const FALCON_ID_INVALID: u32 = 0xFFFF_FFFF;
/// Bootstrap owner: the falcon running the loader, the PMU.
const OWNER_PMU: u32 = 0;
/// `lsb_header`: signature then the 12-word tail.
const LSB_SIZE: u32 = LS_SIG_SIZE as u32 + 48;

/// WPR image for one LS falcon (`gm200_acr_wpr_layout` / `_build`, then
/// `_patch` for a WPR at `wpr_base`): its bootloader DMAs from there.
pub fn wpr(ls: &Ls, falcon_id: u32, wpr_base: u64) -> Vec<u8> {
    let lsb = align(WPR_HEADERS * WPR_HEADER_SIZE, 256);
    let img = align(lsb + LSB_SIZE, 4096);
    let bld = align(img + ls.img.len() as u32, 256);
    let bld_size = align(BL_DMEM_SIZE as u32, 256);
    let mut w = vec![0u8; (bld + bld_size) as usize];

    // WPR header, then the list terminator.
    for (i, v) in [falcon_id, lsb, OWNER_PMU, 0, WPR_STATUS_COPY]
        .iter()
        .enumerate()
    {
        put32(&mut w, 4 * i, *v);
    }
    put32(&mut w, WPR_HEADER_SIZE as usize, FALCON_ID_INVALID);

    // LSB header: signature, then offsets in the WPR.
    let app_start = ls.bl_size;
    let app_size = ls.code_size + ls.data_size;
    let ucode_size = align(ls.code_size, 256) + ls.bl_size;
    let tail = [
        img,
        ucode_size,
        app_size + ls.bl_size - ucode_size,
        ls.bl_size,
        ls.bl_imem_off,
        bld,
        bld_size,
        app_start,
        ls.code_size,
        app_start + ls.code_size,
        ls.data_size,
        0, // flags
    ];
    let l = lsb as usize;
    w[l..l + LS_SIG_SIZE].copy_from_slice(&ls.sig);
    for (i, v) in tail.iter().enumerate() {
        put32(&mut w, l + LS_SIG_SIZE + 4 * i, *v);
    }

    w[img as usize..img as usize + ls.img.len()].copy_from_slice(&ls.img);

    // Bootloader arguments (`gm20b_gr_acr_bld_write`), already at the
    // WPR's physical address.
    let base = wpr_base + (img + app_start) as u64;
    let desc = BlDmem {
        ctx_dma: DMAIDX_UCODE,
        code_dma_base: base >> 8,
        non_sec_code_off: 0,
        non_sec_code_size: ls.code_size,
        sec_code_off: 0,
        sec_code_size: 0,
        code_entry_point: 0,
        data_dma_base: (base + ls.code_size as u64) >> 8,
        data_size: ls.data_size,
    };
    w[bld as usize..bld as usize + BL_DMEM_SIZE].copy_from_slice(&desc.bytes());
    w
}

/// `sw_nonctx`, `sw_bundle_init`, `sw_method_init`: (address, value).
pub fn av(b: &[u8]) -> impl Iterator<Item = (u32, u32)> + '_ {
    b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| (u32_at(c, 0).unwrap(), u32_at(c, 4).unwrap()))
}

/// `sw_ctx`: (address, index, value); the index is unused on GM20B.
pub fn aiv(b: &[u8]) -> impl Iterator<Item = (u32, u32)> + '_ {
    b.as_chunks::<12>()
        .0
        .iter()
        .map(|c| (u32_at(c, 0).unwrap(), u32_at(c, 8).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Option<Vec<u8>> {
        std::fs::read(format!("firmware/nvidia/gm20b/{name}")).ok()
    }

    fn container(header: &[u32], data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 0x20];
        put32(&mut b, 0, BIN_MAGIC);
        put32(&mut b, 12, 0x20);
        put32(&mut b, 16, (0x20 + header.len() * 4) as u32);
        put32(&mut b, 20, data.len() as u32);
        for v in header {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn bootloader() {
        let b = container(
            &[0x5C, 0, 0, 8, 8, 4],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 9, 9, 9],
        );
        let bl = bl(&b).unwrap();
        assert_eq!(bl.boot_addr, 0x5C00);
        assert_eq!(bl.code, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(super::bl(&[0; 32]).is_err());
    }

    #[test]
    fn wpr_layout() {
        let blf = container(&[0x5C, 0, 0, 0x100, 0x100, 0x100], &[0xAA; 0x200]);
        let ls = ls(&blf, &[0x11; 17021], &[0x22; 1964], &[0x33; 76]).unwrap();
        assert_eq!(ls.img.len(), 0x100 + 0x4300 + 0x800);
        let w = wpr(&ls, FALCON_FECS, 0xFF10_0000);
        let r = |o: usize| u32_at(&w, o).unwrap();
        // Header: FECS, LSB at 0x100, owner PMU, not lazy, COPY; terminator.
        assert_eq!(
            [r(0), r(4), r(8), r(12), r(16), r(20)],
            [2, 0x100, 0, 0, 1, !0]
        );
        // LSB tail (nouveau's numbers for the linux-firmware files).
        let t = 0x100 + 76;
        let tail: Vec<u32> = (0..12).map(|i| r(t + 4 * i)).collect();
        assert_eq!(
            tail,
            [
                0x1000, 0x4400, 0x800, 0x100, 0x5C00, 0x5C00, 0x100, 0x100, 0x4300, 0x4400, 0x800,
                0
            ]
        );
        assert_eq!(w[0x1000], 0xAA);
        assert_eq!(w[0x1100], 0x11);
        assert_eq!(w[0x1000 + 0x4400], 0x22);
        // Bootloader arguments: UCODE DMA from the WPR's physical address.
        let d = 0x5C00;
        assert_eq!(r(d + 32), DMAIDX_UCODE);
        assert_eq!(r(d + 36), (0xFF10_0000 + 0x1100) >> 8);
        assert_eq!(r(d + 44), 0x4300);
        assert_eq!(r(d + 60), (0xFF10_0000 + 0x1100 + 0x4300) >> 8);
        assert_eq!(r(d + 64), 0x800);
        assert_eq!(w.len(), 0x5D00);
    }

    #[test]
    fn netlists() {
        let b = [1u8, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0];
        assert_eq!(av(&b).collect::<Vec<_>>(), [(1, 2), (3, 4)]);
        let b = [1u8, 0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(aiv(&b).collect::<Vec<_>>(), [(1, 2)]);
    }

    /// The linux-firmware files, when tools/gm20b-fw.sh ran.
    #[test]
    fn real_files() {
        let (Some(load), Some(acr_bl), Some(fbl)) = (
            file("acr/ucode_load.bin"),
            file("acr/bl.bin"),
            file("gr/fecs_bl.bin"),
        ) else {
            return;
        };
        let mut h = hs(&load).unwrap();
        assert_eq!((h.nmem_base, h.nmem_size), (0, 0x100));
        assert_eq!((h.imem_base, h.imem_size), (0x100, 0x1A00));
        assert_eq!((h.dmem_base_img, h.dmem_size), (0x1B00, 0xC00));
        assert_eq!(h.img.len(), 0x4800);
        // Signatures sit in the container header area: prod 0x50, debug 0x40.
        h.sign(false);
        assert_eq!(h.img[0x2690..0x26A0], load[0x50..0x60]);
        h.sign(true);
        assert_eq!(h.img[0x2690..0x26A0], load[0x40..0x50]);
        h.set_blob(0x1_2345_6700, 0x5C00);
        assert_eq!(u32_at(&h.img, 0x1B00 + 0x240).unwrap(), 0x5C00);
        assert_eq!(u32_at(&h.img, 0x1B00 + 0x248).unwrap(), 0x2345_6700);
        assert_eq!(u32_at(&h.img, 0x1B00 + 0x24C).unwrap(), 1);
        let b = bl(&acr_bl).unwrap();
        assert_eq!((b.boot_addr, b.code.len()), (0x10000, 0x200));
        let (inst, data, sig) = (
            file("gr/fecs_inst.bin").unwrap(),
            file("gr/fecs_data.bin").unwrap(),
            file("gr/fecs_sig.bin").unwrap(),
        );
        let ls = ls(&fbl, &inst, &data, &sig).unwrap();
        assert_eq!((ls.bl_imem_off, ls.bl_size), (0x5C00, 0x100));
        assert_eq!(wpr(&ls, FALCON_FECS, 0).len(), 0x5D00);
        assert_eq!(
            av(&file("gr/sw_nonctx.bin").unwrap()).next(),
            Some((0x400080, 0x9003E2))
        );
        let m: Vec<_> = av(&file("gr/sw_method_init.bin").unwrap())
            .take(1)
            .collect();
        assert_eq!(m, [(0x0200_B197, 0)]);
    }
}
