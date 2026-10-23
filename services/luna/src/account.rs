//! The account side, on a thread of its own: network time, sign-in by
//! code, the token kept fresh, the player profile, then the catalog and
//! its covers, read again every half hour.

use std::collections::HashSet;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lunacore::auth::{self, Session, State as Stored};
use lunacore::catalog::{self, ENTITLEMENT_BATCH};
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn run(cat: Catalog, domain: String, user: String) {
    let mut with_cover: HashSet<String> = HashSet::new();
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
                show(&cat, &games);
                covers(&cat, &mut api, &games, &mut with_cover);
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

fn show(cat: &Catalog, games: &[catalog::Game]) {
    let list = games
        .iter()
        .filter(|g| nxsvc::valid_id(&g.id))
        .map(|g| Game {
            id: g.id.clone(),
            title: g.title.clone(),
            publisher: g.publisher.clone().unwrap_or_default(),
            version: String::new(),
            description: g.genres.join(", "),
            color: 0x232F3E,
        })
        .collect();
    cat.set_games(list);
    let text = if games.is_empty() {
        "No game this account can play"
    } else {
        ""
    };
    cat.set_state(State::Ready(text.into()));
}

/// Each game's box art, in list order (the first ones show first).
fn covers(cat: &Catalog, api: &mut Api, games: &[catalog::Game], done: &mut HashSet<String>) {
    let (mut got, mut first_err) = (0, None);
    for g in games {
        if done.contains(&g.id) {
            continue;
        }
        let url = g
            .portrait
            .as_deref()
            .or(g.landscape.as_deref())
            .and_then(|u| cover::sized_url(u, COVER_SIDE as u32));
        let pic = url
            .ok_or_else(|| "no picture URL".to_string())
            .and_then(|u| api.image(&u))
            .and_then(|b| {
                cover::decode(&b).ok_or_else(|| format!("not a JPEG ({} bytes)", b.len()))
            });
        match pic {
            Ok(p) => {
                let px = cover::square(&p, COVER_SIDE);
                cat.set_cover(
                    &g.id,
                    &Cover::from_fn(COVER_SIDE, |x, y| px[y * COVER_SIDE + x]),
                );
                done.insert(g.id.clone());
                got += 1;
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    println!(
        "luna: {} covers, first failure: {}",
        got,
        first_err.as_deref().unwrap_or("none")
    );
}
