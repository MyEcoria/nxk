//! `getPage` requests, and what is looked for in pages: player profiles,
//! the sign-in page's QR target.

use serde_json::{json, Value};

use crate::config;

/// A `getPage` request (`page_id`: `None` for the default page).
pub fn request(page_type: &str, page_id: Option<&str>) -> Value {
    json!({
        "featureScheme": config::FEATURE_SCHEME,
        "pageContext": {
            "pageId": page_id.unwrap_or(config::DEFAULT_PAGE_ID),
            "pageType": page_type,
        },
    })
}

/// A player profile of the account: its id and name.
#[derive(Clone, PartialEq, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

/// The profiles of a `player_selection` page, in page order.
pub fn profiles(page: &Value) -> Vec<Profile> {
    let mut out = Vec::new();
    walk(page, &mut |m| {
        if m.get("type").and_then(Value::as_str) != Some("PLAYER_SELECTION_TILE") {
            return;
        }
        let Some(data) = m
            .get("presentationData")
            .and_then(Value::as_str)
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
        else {
            return;
        };
        if let Some(id) = data["profileId"].as_str() {
            let name = data["primaryText"].as_str().unwrap_or("").to_string();
            out.push(Profile {
                id: id.to_string(),
                name,
            });
        }
    });
    out
}

/// The profile to use: the one named as the console's user (any case),
/// else the first.
pub fn pick_profile<'a>(all: &'a [Profile], user: &str) -> Option<&'a Profile> {
    all.iter()
        .find(|p| !user.is_empty() && p.name.eq_ignore_ascii_case(user))
        .or_else(|| all.first())
}

/// The profile token of a `player/playerLogin` reply.
pub fn profile_token(reply: &Value) -> Option<&str> {
    reply["token"].as_str()
}

/// The first `qrImage` of a page: the URL Luna has the device show as a QR code.
pub fn qr_url(page: &Value) -> Option<String> {
    let mut found = None;
    walk(page, &mut |m| {
        if found.is_none() {
            found = m.get("qrImage").and_then(Value::as_str).map(str::to_owned);
        }
    });
    found
}

/// Every object of `v`, depth first.
pub(crate) fn walk(v: &Value, f: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
    match v {
        Value::Object(m) => {
            f(m);
            m.values().for_each(|c| walk(c, f));
        }
        Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_the_page() {
        let r = request("code_based_linking", None);
        assert_eq!(r["pageContext"]["pageType"], "code_based_linking");
        assert_eq!(r["pageContext"]["pageId"], "default");
        assert_eq!(
            request("game_detail", Some("p"))["pageContext"]["pageId"],
            "p"
        );
    }

    #[test]
    fn profiles_come_from_tiles_only() {
        let tile = json!({"type":"PLAYER_SELECTION_TILE",
            "presentationData":"{\"profileId\":\"p1\",\"primaryText\":\"Ana\"}"});
        let new =
            json!({"type":"PLAYER_SELECTION_TILE","presentationData":"{\"primaryText\":\"New\"}"});
        let other = json!({"type":"OTHER","presentationData":"{\"profileId\":\"no\"}"});
        let p = profiles(&json!([tile, new, other]));
        assert_eq!(
            p,
            vec![Profile {
                id: "p1".into(),
                name: "Ana".into()
            }]
        );
    }

    #[test]
    fn the_users_profile_wins() {
        let all = vec![
            Profile {
                id: "a".into(),
                name: "Ana".into(),
            },
            Profile {
                id: "b".into(),
                name: "Bob".into(),
            },
        ];
        assert_eq!(pick_profile(&all, "bob").unwrap().id, "b");
        assert_eq!(pick_profile(&all, "Zoe").unwrap().id, "a");
        assert!(pick_profile(&[], "x").is_none());
    }

    #[test]
    fn qr_target_and_token() {
        let page = json!({"a":[{"qrImage": 3}, {"w":{"qrImage":"https://x/y"}}]});
        assert_eq!(qr_url(&page).as_deref(), Some("https://x/y"));
        assert_eq!(qr_url(&json!({})), None);
        assert_eq!(profile_token(&json!({"token":"T"})), Some("T"));
    }
}
