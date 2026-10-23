//! Amazon's code-based linking: a code pair (`/auth/create/codepair`) the
//! user validates on another device, then `/auth/register` for the tokens,
//! `/auth/token` to refresh them. The state kept on the card.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{as_u64, config};

/// Refresh this long before the access token expires.
const EXPIRY_MARGIN_S: u64 = 120;

/// Tokens and account facts of a registered device.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
pub struct Session {
    pub domain: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix time the access token expires.
    pub expires_at: u64,
    pub customer: Customer,
    /// The account's region's proxy (`getAppConfig`).
    #[serde(default)]
    pub proxy: Option<String>,
    /// The player profile in use (`x-amz-profile-token`).
    #[serde(default)]
    pub profile_token: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
pub struct Customer {
    pub name: Option<String>,
    pub country_of_residence: Option<String>,
    pub preferred_marketplace: Option<String>,
}

/// Everything kept on the card: the device's serial, the session.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct State {
    pub device_serial: String,
    pub session: Option<Session>,
}

impl State {
    /// A new device: a 16-hex-digit serial from random bytes.
    pub fn new(random: [u8; 8]) -> Self {
        Self {
            device_serial: format!("{:016x}", u64::from_le_bytes(random)),
            session: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct CodePair {
    pub public_code: String,
    pub private_code: String,
    pub expires_in: u64,
    pub poll_every: u64,
}

pub fn codepair_body(serial: &str) -> Value {
    json!({
        "code_data": {
            "domain": "Device",
            "app_name": config::APP_NAME,
            "app_version": config::APP_VERSION_CODE,
            "device_model": config::DEVICE_MODEL,
            "os_version": "33",
            "device_type": config::MAP_DEVICE_TYPE,
            "device_serial": serial,
        }
    })
}

pub fn parse_codepair(body: &Value) -> Option<CodePair> {
    let text = |k: &str| body.get(k)?.as_str().map(str::to_owned);
    Some(CodePair {
        public_code: text("public_code")?,
        private_code: text("private_code")?,
        expires_in: as_u64(&body["expires_in"])?,
        poll_every: as_u64(&body["polling_interval_in_seconds"])?.max(1),
    })
}

pub fn register_body(serial: &str, domain: &str, pair: &CodePair) -> Value {
    let cookie_domain = if domain.starts_with('.') {
        domain.to_string()
    } else {
        format!(".{}", domain)
    };
    json!({
        "auth_data": { "code_pair": {
            "public_code": pair.public_code, "private_code": pair.private_code } },
        "registration_data": {
            "domain": "DeviceLegacy",
            "device_type": config::MAP_DEVICE_TYPE,
            "device_serial": serial,
            "app_name": config::APP_NAME,
            "app_version": config::APP_VERSION_CODE,
            "device_model": config::DEVICE_MODEL,
            "os_version": config::OS_FINGERPRINT,
            "software_version": config::SOFTWARE_VERSION,
        },
        "requested_token_type": ["bearer", "mac_dms", "store_authentication_cookie", "website_cookies"],
        "cookies": { "domain": cookie_domain, "website_cookies": [] },
        "requested_extensions": ["device_info", "customer_info"],
    })
}

/// A `register` reply: the session, or why not yet (an error code: the
/// code is not validated yet, or Amazon refused).
pub fn parse_register(domain: &str, body: &Value, now: u64) -> Result<Session, String> {
    let r = &body["response"];
    if let Some(s) = r.get("success").and_then(|s| session_of(domain, s, now)) {
        return Ok(s);
    }
    let detail = r.get("error").or_else(|| r.get("challenge"));
    let field = |k: &str| detail.and_then(|d| d.get(k)).and_then(Value::as_str);
    Err(field("code")
        .or_else(|| field("challenge_reason"))
        .unwrap_or("no answer")
        .to_string())
}

fn session_of(domain: &str, success: &Value, now: u64) -> Option<Session> {
    let bearer = &success["tokens"]["bearer"];
    let info = &success["extensions"]["customer_info"];
    let text = |v: &Value| v.as_str().map(str::to_owned);
    Some(Session {
        domain: domain.to_string(),
        access_token: text(&bearer["access_token"])?,
        refresh_token: text(&bearer["refresh_token"])?,
        expires_at: now + as_u64(&bearer["expires_in"]).unwrap_or(3600),
        customer: Customer {
            name: text(&info["name"]),
            country_of_residence: text(&info["country_of_residence"]),
            preferred_marketplace: text(&info["preferred_marketplace"]),
        },
        proxy: None,
        profile_token: None,
    })
}

pub fn needs_refresh(s: &Session, now: u64) -> bool {
    now + EXPIRY_MARGIN_S >= s.expires_at
}

pub fn refresh_body(refresh_token: &str) -> Value {
    json!({
        "app_name": config::APP_NAME,
        "app_version": config::APP_VERSION_CODE,
        "source_token_type": "refresh_token",
        "source_token": refresh_token,
        "requested_token_type": "access_token",
    })
}

/// The session with a `/auth/token` reply's new token; `None` without one.
pub fn refreshed(s: &Session, reply: &Value, now: u64) -> Option<Session> {
    let mut n = s.clone();
    n.access_token = reply["access_token"].as_str()?.to_string();
    n.expires_at = now + as_u64(&reply["expires_in"]).unwrap_or(3600);
    if let Some(rt) = reply["refresh_token"].as_str() {
        n.refresh_token = rt.to_string();
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> CodePair {
        CodePair {
            public_code: "PUB".into(),
            private_code: "PRIV".into(),
            expires_in: 600,
            poll_every: 1,
        }
    }

    #[test]
    fn codepair_body_is_the_android_clients() {
        let b = codepair_body("SER");
        assert_eq!(b["code_data"]["device_type"], "A1MPSLFC7L5AFK");
        assert_eq!(b["code_data"]["device_serial"], "SER");
        assert_eq!(b["code_data"]["app_name"], "com.amazon.spiderpork");
    }

    #[test]
    fn codepair_numbers_as_strings_or_numbers() {
        let p = parse_codepair(&json!({"public_code":"ABC123","private_code":"p",
            "expires_in":"600","polling_interval_in_seconds":5}))
        .unwrap();
        assert_eq!(p.public_code, "ABC123");
        assert_eq!((p.expires_in, p.poll_every), (600, 5));
        assert!(parse_codepair(&json!({"public_code":"x"})).is_none());
    }

    #[test]
    fn register_body_carries_the_pair() {
        let b = register_body("SER", "amazon.fr", &pair());
        assert_eq!(b["auth_data"]["code_pair"]["private_code"], "PRIV");
        assert_eq!(b["registration_data"]["device_type"], "A1MPSLFC7L5AFK");
        assert_eq!(b["cookies"]["domain"], ".amazon.fr");
    }

    #[test]
    fn register_success_gives_a_session() {
        let body = json!({"response":{"success":{
            "tokens":{"bearer":{"access_token":"AT","refresh_token":"RT","expires_in":"3600"}},
            "extensions":{"customer_info":{"name":"Jean","country_of_residence":"FR",
                "preferred_marketplace":"A13V1IB3VIYZZH"}}}}});
        let s = parse_register("amazon.fr", &body, 1000).unwrap();
        assert_eq!((s.access_token.as_str(), s.expires_at), ("AT", 4600));
        assert_eq!(s.customer.country_of_residence.as_deref(), Some("FR"));
    }

    #[test]
    fn register_pending_gives_the_code() {
        let e = json!({"response":{"error":{"code":"InvalidAuthenticationData"}}});
        assert_eq!(
            parse_register("amazon.com", &e, 0).unwrap_err(),
            "InvalidAuthenticationData"
        );
        let c = json!({"response":{"challenge":{"challenge_reason":"AuthenticationFailed"}}});
        assert_eq!(
            parse_register("amazon.com", &c, 0).unwrap_err(),
            "AuthenticationFailed"
        );
        assert!(parse_register("amazon.com", &Value::Null, 0).is_err());
    }

    #[test]
    fn refresh_keeps_or_replaces_the_refresh_token() {
        let s = Session {
            refresh_token: "RT".into(),
            expires_at: 10,
            ..Session::default()
        };
        assert!(needs_refresh(&s, 0));
        let n = refreshed(&s, &json!({"access_token":"A2","expires_in":100}), 5).unwrap();
        assert_eq!(
            (
                n.access_token.as_str(),
                n.refresh_token.as_str(),
                n.expires_at
            ),
            ("A2", "RT", 105)
        );
        assert!(refreshed(&s, &json!({}), 5).is_none());
        assert_eq!(refresh_body("RT")["source_token"], "RT");
    }

    #[test]
    fn serials_are_16_hex_digits() {
        let st = State::new([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(st.device_serial, "0807060504030201");
    }
}
