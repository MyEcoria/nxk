//! What the Luna Android client says about itself (docs/ANALYSIS.md of the
//! desktop client), and the hosts it talks to.

/// `app_name` for Amazon's identity service.
pub const APP_NAME: &str = "com.amazon.spiderpork";
/// Android `versionCode`, `app_version` for the identity service.
pub const APP_VERSION_CODE: &str = "140002110";
/// `x-amz-client-version` on Luna's proxy.
pub const CLIENT_VERSION: &str = "1.14002.0021.0-release";
/// Device type of a non-Amazon Android device for the identity service
/// (`DEVICE_TYPE_3P`): the Luna type there gives `InvalidDevice`.
pub const MAP_DEVICE_TYPE: &str = "A1MPSLFC7L5AFK";
/// Device type the Luna client sends its own proxy.
pub const PROXY_DEVICE_TYPE: &str = "A226NK9DE5GSG6";
/// Luna's proxy until `getAppConfig` names the account's region's.
pub const DEFAULT_PROXY: &str = "proxy-prod.tempo.digital.a2z.com";
pub const FEATURE_SCHEME: &str = "TV_V1";
pub const PLATFORM: &str = "ANDROID_TV";
pub const DEFAULT_PAGE_ID: &str = "default";
// Not observed: plausible values of an Android TV box.
pub const DEVICE_MODEL: &str = "Pixel 6";
pub const OS_RELEASE: &str = "13";
pub const OS_FINGERPRINT: &str =
    "google/oriole/oriole:13/TQ3A.230901.001/10750268:user/release-keys";
pub const SOFTWARE_VERSION: &str = "1";

/// The identity service's host for an Amazon domain (`amazon.fr` gives
/// `api.amazon.fr`; empty means `.com`).
pub fn panda_host(domain: &str) -> String {
    let d = domain.trim();
    if d.is_empty() {
        "api.amazon.com".into()
    } else if let Some(rest) = d.strip_prefix('.') {
        format!("api.{}", rest)
    } else if d.starts_with("api.") {
        d.into()
    } else if let Some(rest) = d.strip_prefix("www.") {
        format!("api.{}", rest)
    } else {
        format!("api.{}", d)
    }
}

/// The identity library's User-Agent.
pub fn panda_user_agent() -> String {
    format!(
        "AmazonWebView/MAPClientLib/130050002/Android/{}/{}",
        OS_RELEASE, DEVICE_MODEL
    )
}

/// Where a code is typed when Luna gives no page to scan.
pub fn code_page(domain: &str) -> String {
    let d = domain.trim().trim_start_matches('.');
    let d = d.strip_prefix("www.").unwrap_or(d);
    format!(
        "https://www.{}/code",
        if d.is_empty() { "amazon.com" } else { d }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panda_hosts() {
        assert_eq!(panda_host(""), "api.amazon.com");
        assert_eq!(panda_host("amazon.fr"), "api.amazon.fr");
        assert_eq!(panda_host(".amazon.de"), "api.amazon.de");
        assert_eq!(panda_host("api.amazon.com"), "api.amazon.com");
        assert_eq!(panda_host("www.amazon.co.uk"), "api.amazon.co.uk");
    }

    #[test]
    fn code_pages() {
        assert_eq!(code_page("amazon.fr"), "https://www.amazon.fr/code");
        assert_eq!(code_page(""), "https://www.amazon.com/code");
    }
}
