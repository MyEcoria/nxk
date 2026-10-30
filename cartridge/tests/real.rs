//! The title of a real NSP's Control NCA, read with the console's own
//! `prod.keys`. Skipped unless `NXK_NSP` names an NSP; the keys come from
//! `NXK_PROD_KEYS` or `firmware/keys/prod.keys` (both git-ignored).

use nxk_cartridge::source::Source;
use nxk_cartridge::{nacp, title_name, Error, Keys};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

/// Random access to a file (the NSP is far too large to read whole).
struct FileSource(File);

impl Source for FileSource {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> nxk_cartridge::Result<()> {
        self.0.read_exact_at(buf, off).map_err(|_| Error::Io)
    }
}

fn keys_path() -> PathBuf {
    match std::env::var_os("NXK_PROD_KEYS") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../firmware/keys/prod.keys"),
    }
}

#[test]
fn reads_the_title_of_a_real_nsp() {
    let (Some(nsp), Ok(keys_text)) = (std::env::var_os("NXK_NSP"), std::fs::read(keys_path()))
    else {
        eprintln!("skipped: set NXK_NSP and provide prod.keys");
        return;
    };
    let keys = Keys::parse(&keys_text);
    let file = FileSource(File::open(nsp).unwrap());
    let mut out = [0u8; nacp::NAME_LEN];
    let n = title_name(&keys, &file, 0, &mut out).unwrap();
    assert_eq!(
        std::str::from_utf8(&out[..n]).unwrap(),
        "The Legend of Zelda: Tears of the Kingdom"
    );
}
