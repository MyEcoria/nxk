//! The account side, on a thread of its own: the library kept on the card
//! shown at once, network time, sign-in by code, the token kept fresh, the
//! player profile, then the catalog and its covers (only the new ones are
//! downloaded), read again every half hour.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lunacore::auth::{self, Session, State as Stored};
use lunacore::catalog::{self, ENTITLEMENT_BATCH};
use lunacore::library::{self, Library};
use lunacore::{config, cover, pages};
use nxsvc::{Catalog, Cover, Game, State};
use serde_json::json;

use crate::api::{status_of, Api, Result};
use crate::{qr, store};

/// Certificates cannot be checked before the clock is past this (`power`
/// sets it from the network): 2026-10-01.
const TIME_FLOOR_S: u64 = 1_790_812_800;
const RETRY: Duration = Duration::from_secs(15);
const REFRESH: Duration = Duration::from_secs(30 * 60);
const COVER_SIDE: usize = 256;

/// The address each game's cover on show was made from.
type Shown = HashMap<String, String>;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn run(cat: Catalog, domain: String, user: String) {
    let dir = store::dir();
    let dir = Path::new(&dir);
    let mut lib = library::load(dir);
    let mut shown = Shown::new();
    if !lib.games.is_empty() {
        println!("luna: {} games on the card", lib.games.len());
        list(&cat, &lib);
        from_card(&cat, dir, &lib, &mut shown);
    }
    loop {
        while unix_now() < TIME_FLOOR_S {
            cat.set_state(State::Loading("Waiting for the network time".into()));
            sleep(Duration::from_secs(2));
        }
        let mut st = store::load();
        let mut api = Api::new(&st.device_serial);
        let games = signed_in(&cat, &mut api, &mut st, &domain, &user)
            .and_then(|()| read_catalog(&mut api, &mut st));
        let wait = match games {
            Ok(games) => {
                let new = Library::of(&games, COVER_SIDE as u32);
                list(&cat, &new);
                cat.set_state(State::Ready(if games.is_empty() {
                    "No game this account can play".into()
                } else {
                    String::new()
                }));
                shown.retain(|id, _| new.has(id));
                library::prune(dir, &lib, &new);
                if library::save(dir, &new).is_err() {
                    println!("luna: the library could not be kept on the card");
                }
                lib = new;
                covers(&cat, &mut api, dir, &lib, &mut shown);
                REFRESH
            }
            Err(e) => {
                println!("luna: {}", e);
                cat.set_state(State::Error(e));
                RETRY
            }
        };
        sleep(wait);
    }
}

/// A session with a fresh token, a regional proxy and a player profile.
fn signed_in(
    cat: &Catalog,
    api: &mut Api,
    st: &mut Stored,
    domain: &str,
    user: &str,
) -> Result<()> {
    if st.session.is_none() {
        let s = login(cat, api, domain)?;
        println!("luna: signed in");
        st.session = Some(s);
        store::save(st);
    }
    let s = st.session.as_mut().expect("session");
    if auth::needs_refresh(s, unix_now()) {
        cat.set_state(State::Loading("Signing in".into()));
        let (code, reply) = api.panda(
            &s.domain,
            "/auth/token",
            &auth::refresh_body(&s.refresh_token),
        )?;
        match (code, auth::refreshed(s, &reply, unix_now())) {
            (200, Some(n)) => *s = n,
            // The device was removed from the account: sign in again.
            (400 | 401, _) => {
                st.session = None;
                store::save(st);
                return Err("Signed out by Amazon: sign in again".into());
            }
            _ => return Err(format!("auth/token: HTTP {}", code)),
        }
        store::save(st);
    }
    let s = st.session.as_mut().expect("session");
    if s.profile_token.is_none() {
        cat.set_state(State::Loading("Choosing the player".into()));
        let cfg = api.call(Some(s), "getAppConfig", &json!({}))?;
        s.proxy = cfg["endpoints"]["clientProxy"].as_str().map(str::to_owned);
        let page = api.call(
            Some(s),
            "getPage",
            &pages::request("player_selection", None),
        )?;
        let all = pages::profiles(&page);
        let p = pages::pick_profile(&all, user).ok_or("No Luna player profile on the account")?;
        let reply = api.call(Some(s), "player/playerLogin", &json!({ "profileId": p.id }))?;
        s.profile_token = Some(
            pages::profile_token(&reply)
                .ok_or("playerLogin: no token")?
                .to_string(),
        );
        store::save(st);
    }
    Ok(())
}

/// Code-based linking: show a code and its page (and a QR code of the
/// page Luna names), wait for the user to enter it, new codes as they
/// expire.
fn login(cat: &Catalog, api: &mut Api, domain: &str) -> Result<Session> {
    let page = config::code_page(domain);
    let target = api
        .call(None, "getPage", &pages::request("code_based_linking", None))
        .ok()
        .and_then(|p| pages::qr_url(&p))
        .unwrap_or_else(|| page.clone());
    let shown = page
        .trim_start_matches("https://")
        .trim_start_matches("www.")
        .to_string();
    loop {
        let (code, reply) = api.panda(
            domain,
            "/auth/create/codepair",
            &auth::codepair_body(&api.serial.clone()),
        )?;
        if code != 200 {
            return Err(format!("auth/create/codepair: HTTP {}", code));
        }
        let pair = auth::parse_codepair(&reply).ok_or("auth/create/codepair: no code")?;
        cat.set_login_art(qr::image(&target).as_ref());
        cat.set_state(State::Login(format!("{}\n{}", pair.public_code, shown)));
        let end = unix_now() + pair.expires_in;
        let body = auth::register_body(&api.serial.clone(), domain, &pair);
        while unix_now() + pair.poll_every < end {
            sleep(Duration::from_secs(pair.poll_every));
            // Until the code is entered the answer is an error: wait on.
            if let Ok((_, reply)) = api.panda(domain, "/auth/register", &body) {
                if let Ok(s) = auth::parse_register(domain, &reply, unix_now()) {
                    cat.set_login_art(None);
                    return Ok(s);
                }
            }
        }
    }
}

/// The games the account can play (a token refused: renewed next time).
fn read_catalog(api: &mut Api, st: &mut Stored) -> Result<Vec<catalog::Game>> {
    let s = st.session.clone().expect("session");
    let got = (|| -> Result<catalog::Catalog> {
        let home = api.call(
            Some(&s),
            "getPage",
            &pages::request(catalog::HOME_PAGE, None),
        )?;
        let browse = api.call(
            Some(&s),
            "getPage",
            &pages::request(catalog::BROWSE_PAGE, None),
        )?;
        let mut ok = HashSet::new();
        for batch in catalog::all_ids(&[&home, &browse]).chunks(ENTITLEMENT_BATCH) {
            let r = api.call(
                Some(&s),
                "getProductEntitlementStatuses",
                &catalog::entitlement_request(batch),
            )?;
            ok.extend(catalog::entitled(&r));
        }
        Ok(catalog::build(&home, &browse, &ok))
    })();
    match got {
        Ok(c) => {
            println!("luna: {} games ({} not playable)", c.games.len(), c.hidden);
            Ok(c.games)
        }
        Err(e) => {
            if matches!(status_of(&e), Some(401 | 403)) {
                let n = st.session.as_mut().expect("session");
                (n.expires_at, n.profile_token) = (0, None);
                store::save(st);
            }
            Err(e)
        }
    }
}

/// The games on the home screen (the state is the caller's).
fn list(cat: &Catalog, lib: &Library) {
    let games = lib
        .games
        .iter()
        .filter(|e| nxsvc::valid_id(&e.id))
        .map(|e| Game {
            id: e.id.clone(),
            title: e.title.clone(),
            publisher: e.publisher.clone(),
            version: String::new(),
            description: e.genres.join(", "),
            color: 0x232F3E,
        })
        .collect();
    cat.set_games(games);
}

/// The cover made of a decoded picture: the middle of it, `COVER_SIDE` square.
fn make(p: &cover::Picture) -> Cover {
    let px = cover::square(p, COVER_SIDE);
    Cover::from_fn(COVER_SIDE, |x, y| px[y * COVER_SIDE + x])
}

/// The covers kept on the card, from the first game on.
fn from_card(cat: &Catalog, dir: &Path, lib: &Library, shown: &mut Shown) {
    for e in &lib.games {
        let pic = library::read_cover(dir, e).and_then(|b| cover::decode(&b));
        if let (Some(p), Some(url)) = (pic, &e.cover_url) {
            cat.set_cover(&e.id, &make(&p));
            shown.insert(e.id.clone(), url.clone());
        }
    }
    println!("luna: {} covers from the card", shown.len());
}

/// Each game's box art, in list order (the first ones show first); the
/// ones on show already, from the same picture, are not asked for again.
fn covers(cat: &Catalog, api: &mut Api, dir: &Path, lib: &Library, shown: &mut Shown) {
    let (mut got, mut first_err) = (0, None);
    for e in &lib.games {
        let Some(url) = &e.cover_url else {
            first_err.get_or_insert("no picture URL".to_string());
            continue;
        };
        if shown.get(&e.id) == Some(url) {
            continue;
        }
        let pic = api.image(url).and_then(|b| match cover::decode(&b) {
            Some(p) => Ok((b, p)),
            None => Err(format!("not a JPEG ({} bytes)", b.len())),
        });
        match pic {
            Ok((jpeg, p)) => {
                cat.set_cover(&e.id, &make(&p));
                shown.insert(e.id.clone(), url.clone());
                if library::write_cover(dir, e, &jpeg).is_err() {
                    first_err.get_or_insert("a cover could not be kept".to_string());
                }
                got += 1;
            }
            Err(err) => {
                first_err.get_or_insert(err);
            }
        }
    }
    println!(
        "luna: {} new covers, first failure: {}",
        got,
        first_err.as_deref().unwrap_or("none")
    );
}
