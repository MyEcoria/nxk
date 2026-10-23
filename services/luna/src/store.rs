//! The device's serial and the session, in `state.json` of the user's
//! directory for this catalog (`NXK_SERVICE_DIR`). FAT keeps no
//! permissions: anyone with the card can read the tokens.

use lunacore::auth::State;

fn path() -> String {
    let dir =
        std::env::var(nxk_abi::proto::provider::DIR_ENV).unwrap_or_else(|_| "/config/luna".into());
    format!("{}/state.json", dir.trim_end_matches('/'))
}

/// The state on the card, or a new device.
pub fn load() -> State {
    let p = path();
    let saved = std::fs::read(&p)
        .or_else(|_| std::fs::read(format!("{}.new", p)))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    saved.unwrap_or_else(|| {
        let mut r = [0u8; 8];
        random(&mut r);
        let s = State::new(r);
        save(&s);
        s
    })
}

/// Written whole beside, then put in place: a cut never leaves half a file.
pub fn save(s: &State) {
    let p = path();
    let new = format!("{}.new", p);
    let Ok(text) = serde_json::to_vec_pretty(s) else {
        return;
    };
    if std::fs::write(&new, text).is_ok() {
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::rename(&new, &p);
    }
}

fn random(buf: &mut [u8]) {
    unsafe {
        nxk_abi::user::syscall2(
            nxk_abi::sys::GETRANDOM,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
}
