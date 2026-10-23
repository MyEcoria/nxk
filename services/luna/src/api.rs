//! Calls to Amazon's identity service and Luna's proxy over `nxhttp`.
//! Errors carry the call and the HTTP status only: a body can hold a
//! token, and these messages reach the screen and the logs.

use std::collections::HashMap;
use std::time::Duration;

use lunacore::auth::Session;
use lunacore::{config, cover, proxy};
use nxhttp::Client;
use serde_json::Value;

pub type Result<T> = std::result::Result<T, String>;

/// An HTTP status that is not a success.
pub fn status_of(e: &str) -> Option<u16> {
    e.rsplit("HTTP ").next()?.parse().ok()
}

pub struct Api {
    pub serial: String,
    panda: Option<Client>,
    proxy: Option<Client>,
    images: HashMap<String, Client>,
    /// Connections and reads give up past this (`None`: nxhttp's own).
    timeout: Option<Duration>,
}

impl Api {
    pub fn new(serial: &str) -> Self {
        Self {
            serial: serial.into(),
            panda: None,
            proxy: None,
            images: HashMap::new(),
            timeout: None,
        }
    }

    /// One whose calls give up past `t` (a session to end before a deadline).
    pub fn quick(serial: &str, t: Duration) -> Self {
        Self {
            timeout: Some(t),
            ..Self::new(serial)
        }
    }

    fn client(&self, host: &str) -> Client {
        match self.timeout {
            Some(t) => Client::with_timeout(host, t),
            None => Client::new(host),
        }
    }

    /// How long a GET of `url` takes (a region's ping), within `t`.
    pub fn ping(url: &str, t: Duration) -> Option<Duration> {
        let (host, path) = nxhttp::split_url(url)?;
        let started = std::time::Instant::now();
        let r = Client::with_timeout(host, t).get(path, &[]).ok()?;
        (r.status < 500).then(|| started.elapsed())
    }

    fn post(c: &mut Client, path: &str, headers: &[(&str, String)], body: &Value) -> Result<Value> {
        let h: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let what = path.trim_start_matches('/');
        let r = c
            .post(path, &h, body.to_string().as_bytes())
            .map_err(|e| format!("{}: {}", what, e))?;
        if !(200..300).contains(&r.status) {
            return Err(format!("{}: HTTP {}", what, r.status));
        }
        serde_json::from_slice(&r.body).map_err(|_| format!("{}: not JSON", what))
    }

    /// A call to the identity service of `domain`; its reply even when it
    /// is an error (`register` answers 4xx while the code waits).
    pub fn panda(&mut self, domain: &str, path: &str, body: &Value) -> Result<(u16, Value)> {
        let host = config::panda_host(domain);
        if self.panda.as_ref().is_none_or(|c| c.host() != host) {
            self.panda = Some(self.client(&host));
        }
        let c = self.panda.as_mut().expect("panda client");
        let h = proxy::panda_headers(&host);
        let h: Vec<(&str, &str)> = h.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let r = c
            .post(path, &h, body.to_string().as_bytes())
            .map_err(|e| format!("{}: {}", path.trim_start_matches('/'), e))?;
        let v = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        Ok((r.status, v))
    }

    /// A call to Luna's proxy (refused if it could spend money).
    pub fn call(&mut self, session: Option<&Session>, op: &str, body: &Value) -> Result<Value> {
        if proxy::is_money_operation(op) {
            return Err(format!("{}: refused, nothing here buys", op));
        }
        let host = session
            .and_then(|s| s.proxy.as_deref())
            .unwrap_or(config::DEFAULT_PROXY)
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        if self.proxy.as_ref().is_none_or(|c| c.host() != host) {
            self.proxy = Some(self.client(&host));
        }
        let headers = proxy::headers(&self.serial, session);
        let c = self.proxy.as_mut().expect("proxy client");
        Self::post(c, &format!("/{}", op), &headers, body)
    }

    /// An image (JPEG, `cover::MAX_BYTES` at most), one redirect followed.
    pub fn image(&mut self, url: &str) -> Result<Vec<u8>> {
        let mut url = url.to_string();
        for _ in 0..2 {
            let (host, path) = nxhttp::split_url(&url).ok_or("cover: not https")?;
            let c = self
                .images
                .entry(host.to_string())
                .or_insert_with(|| Client::new(host));
            let r = c.get(path, &[]).map_err(|e| format!("cover: {}", e))?;
            match r.status {
                200 if r.body.len() <= cover::MAX_BYTES => return Ok(r.body),
                301 | 302 | 307 | 308 => {
                    url = r
                        .header("location")
                        .ok_or("cover: no location")?
                        .to_string();
                }
                s => return Err(format!("cover: HTTP {}", s)),
            }
        }
        Err("cover: too many redirects".into())
    }
}
