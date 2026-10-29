//! The TLS 1.2 client inside PEAP: rustls (no_std, RustCrypto) driven
//! by bytes in and out. rustls has no keying-material exporter without
//! `std`, so the PEAP keys are rebuilt as RFC 5705 defines them: the
//! suite's PRF over the master secret (from rustls's key log) and both
//! randoms (the server's from its plaintext ServerHello).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{Resumption, UnbufferedClientConnection};
use rustls::crypto::{verify_tls12_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, KeyLog, SignatureScheme,
    SupportedCipherSuite,
};
use sha2::{Digest, Sha256};
use spin::Mutex;

/// Bytes of keying material PEAP takes (MSK then EMSK).
pub const KEY_LEN: usize = 128;
const RECORD_HANDSHAKE: u8 = 22;
const SERVER_HELLO: u8 = 2;

/// The leaf certificate check: its SHA-256 among the pins, or any leaf
/// when there are none (its hash is kept so it can be pinned).
#[derive(Debug)]
struct Pinned {
    pins: Vec<[u8; 32]>,
    algs: WebPkiSupportedAlgorithms,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let hash: [u8; 32] = Sha256::digest(leaf.as_ref()).into();
        *self.seen.lock() = Some(hash);
        if self.pins.is_empty() || self.pins.contains(&hash) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 is not offered".to_string()))
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Where rustls's key log puts the TLS 1.2 master secret.
#[derive(Debug, Default)]
struct Secrets(Mutex<Option<([u8; 32], [u8; 48])>>);

impl KeyLog for Secrets {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        if label == "CLIENT_RANDOM" && client_random.len() == 32 && secret.len() == 48 {
            let (mut c, mut s) = ([0u8; 32], [0u8; 48]);
            c.copy_from_slice(client_random);
            s.copy_from_slice(secret);
            *self.0.lock() = Some((c, s));
        }
    }

    fn will_log(&self, label: &str) -> bool {
        label == "CLIENT_RANDOM"
    }
}

/// No clock is needed: the pin replaces the validity dates.
#[derive(Debug)]
struct NoClock;

impl TimeProvider for NoClock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::ZERO))
    }
}

pub struct Tls {
    conn: UnbufferedClientConnection,
    secrets: Arc<Secrets>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    incoming: Vec<u8>,
    /// Handshake-record payload from the server until the ServerHello's
    /// random is in.
    hello: Vec<u8>,
    hello_raw: Vec<u8>,
    server_random: Option<[u8; 32]>,
}

fn err(e: Error) -> String {
    alloc::format!("TLS: {}", e)
}

impl Tls {
    /// A client that accepts the leaf certificates in `pins` (SHA-256 of
    /// the DER), or any if empty. Randomness comes from `getrandom`.
    pub fn new(pins: &[[u8; 32]]) -> Result<Self, String> {
        let provider = Arc::new(rustls_rustcrypto::provider());
        let seen = Arc::new(Mutex::new(None));
        let verifier = Pinned {
            pins: pins.to_vec(),
            algs: provider.signature_verification_algorithms,
            seen: seen.clone(),
        };
        let secrets = Arc::new(Secrets::default());
        let mut config = ClientConfig::builder_with_details(provider, Arc::new(NoClock))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.resumption = Resumption::disabled();
        config.enable_sni = false;
        config.key_log = secrets.clone();
        let name = ServerName::try_from("radius").map_err(|_| String::from("TLS: server name"))?;
        let conn = UnbufferedClientConnection::new(Arc::new(config), name).map_err(err)?;
        Ok(Self {
            conn,
            secrets,
            seen,
            incoming: Vec::new(),
            hello: Vec::new(),
            hello_raw: Vec::new(),
            server_random: None,
        })
    }

    /// SHA-256 of the server's leaf certificate, once it was sent.
    pub fn server_cert(&self) -> Option<[u8; 32]> {
        *self.seen.lock()
    }

    pub fn handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// TLS bytes from the server in, TLS bytes for it out, with the
    /// application data that came. The first call takes no input and
    /// yields the ClientHello.
    pub fn feed(&mut self, data: &[u8]) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
        if self.server_random.is_none() {
            self.sniff(data);
        }
        self.incoming.extend_from_slice(data);
        let (mut out, mut app) = (Vec::new(), Vec::new());
        loop {
            let UnbufferedStatus { discard, state } =
                self.conn.process_tls_records(&mut self.incoming);
            let mut stop = false;
            match state.map_err(err)? {
                ConnectionState::EncodeTlsData(mut s) => {
                    let mut buf = vec![0u8; 2048];
                    let n = loop {
                        match s.encode(&mut buf) {
                            Ok(n) => break n,
                            Err(EncodeError::InsufficientSize(e)) => buf.resize(e.required_size, 0),
                            Err(e) => return Err(alloc::format!("TLS: {:?}", e)),
                        }
                    };
                    out.extend_from_slice(&buf[..n]);
                }
                ConnectionState::TransmitTlsData(t) => t.done(),
                ConnectionState::ReadTraffic(mut r) => {
                    while let Some(rec) = r.next_record() {
                        app.push(rec.map_err(err)?.payload.to_vec());
                    }
                }
                ConnectionState::Closed | ConnectionState::PeerClosed => {
                    return Err(String::from("TLS: closed by the server"))
                }
                _ => stop = true,
            }
            self.incoming.drain(..discard);
            if stop {
                return Ok((out, app));
            }
        }
    }

    /// Application data to TLS records, once the handshake is over.
    pub fn encrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let UnbufferedStatus { discard, state } = self.conn.process_tls_records(&mut self.incoming);
        let out = match state.map_err(err)? {
            ConnectionState::WriteTraffic(mut w) => {
                let mut buf = vec![0u8; data.len() + 64];
                loop {
                    match w.encrypt(data, &mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            break Ok(buf);
                        }
                        Err(EncryptError::InsufficientSize(e)) => buf.resize(e.required_size, 0),
                        Err(e) => break Err(alloc::format!("TLS: {:?}", e)),
                    }
                }
            }
            s => Err(alloc::format!("TLS: cannot send now ({:?})", s)),
        };
        self.incoming.drain(..discard);
        out
    }

    /// The server random, from the payload of its first handshake
    /// records (the ServerHello may span records and EAP fragments).
    fn sniff(&mut self, data: &[u8]) {
        self.hello_raw.extend_from_slice(data);
        self.hello.clear();
        let mut k = 0;
        while k + 5 <= self.hello_raw.len() {
            let len = u16::from_be_bytes([self.hello_raw[k + 3], self.hello_raw[k + 4]]) as usize;
            let end = (k + 5 + len).min(self.hello_raw.len());
            if self.hello_raw[k] == RECORD_HANDSHAKE {
                self.hello.extend_from_slice(&self.hello_raw[k + 5..end]);
            }
            k += 5 + len;
        }
        // Handshake header (4), version (2), random (32).
        if self.hello.len() >= 38 && self.hello[0] == SERVER_HELLO {
            let mut r = [0u8; 32];
            r.copy_from_slice(&self.hello[6..38]);
            self.server_random = Some(r);
            self.hello_raw = Vec::new();
            self.hello = Vec::new();
        }
    }

    /// RFC 5705 keying material, no context: the suite's PRF over the
    /// master secret, `label` and client random then server random.
    pub fn export(&self, label: &[u8]) -> Option<Box<[u8; KEY_LEN]>> {
        if self.handshaking() {
            return None;
        }
        let (client, master) = (*self.secrets.0.lock())?;
        let server = self.server_random?;
        let SupportedCipherSuite::Tls12(suite) = self.conn.negotiated_cipher_suite()? else {
            return None;
        };
        let mut seed = [0u8; 64];
        seed[..32].copy_from_slice(&client);
        seed[32..].copy_from_slice(&server);
        let mut out = Box::new([0u8; KEY_LEN]);
        suite
            .prf_provider
            .for_secret(&mut out[..], &master, label, &seed);
        Some(out)
    }
}
