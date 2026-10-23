//! Luna's proxy: every call is `POST /OPERATION` with a JSON body and the
//! headers the Android client's interceptor sets. Calls that can spend
//! money or change payment settings are refused: nothing here buys.

use crate::auth::Session;
use crate::config;

/// Whether `op` can spend money or touch payment or shipping settings.
pub fn is_money_operation(op: &str) -> bool {
    let op = op.to_ascii_lowercase();
    ["purchase", "payment", "shipping", "claimoffer"]
        .iter()
        .any(|w| op.contains(w))
}

/// The headers of a proxy call (`session`: none before the sign-in).
pub fn headers(serial: &str, session: Option<&Session>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("accept-language", "en-US".into()),
        ("x-amz-client-version", config::CLIENT_VERSION.into()),
        ("content-type", "application/json".into()),
        ("x-amz-locale", "en_US".into()),
        ("x-amz-platform", config::PLATFORM.into()),
        ("x-amz-timezone", "UTC".into()),
        ("x-amz-device-serial-number", serial.into()),
        ("x-amz-device-type", config::PROXY_DEVICE_TYPE.into()),
    ];
    let Some(s) = session else {
        return h;
    };
    h.push(("x-amz-access-token", s.access_token.clone()));
    let optional = [
        (
            "x-amz-country-of-residence",
            &s.customer.country_of_residence,
        ),
        ("x-amz-marketplace-id", &s.customer.preferred_marketplace),
        ("x-amz-profile-token", &s.profile_token),
    ];
    for (n, v) in optional {
        if let Some(v) = v {
            h.push((n, v.clone()));
        }
    }
    h
}

/// The identity service's headers.
pub fn panda_headers(host: &str) -> Vec<(&'static str, String)> {
    vec![
        ("accept", "application/json".into()),
        ("accept-language", "en-US".into()),
        ("content-type", "application/json".into()),
        ("user-agent", config::panda_user_agent()),
        ("x-amzn-identity-auth-domain", host.into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_operations_are_refused() {
        assert!(is_money_operation("purchase"));
        assert!(is_money_operation("setDefaultPaymentMethod"));
        assert!(is_money_operation("claimOffer"));
        assert!(is_money_operation("updatePurchaseTransactionState"));
        assert!(!is_money_operation("getPage"));
        assert!(!is_money_operation("getProductEntitlementStatuses"));
    }

    #[test]
    fn headers_match_the_android_interceptor() {
        let mut s = Session {
            access_token: "AT".into(),
            ..Session::default()
        };
        s.customer.country_of_residence = Some("FR".into());
        let h = headers("SER", Some(&s));
        let get = |n: &str| h.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str());
        assert_eq!(get("x-amz-access-token"), Some("AT"));
        assert_eq!(get("x-amz-device-type"), Some("A226NK9DE5GSG6"));
        assert_eq!(get("x-amz-country-of-residence"), Some("FR"));
        assert_eq!(get("x-amz-platform"), Some("ANDROID_TV"));
        assert_eq!(get("x-amz-profile-token"), None);
        assert!(headers("SER", None)
            .iter()
            .all(|(k, _)| *k != "x-amz-access-token"));
    }
}
