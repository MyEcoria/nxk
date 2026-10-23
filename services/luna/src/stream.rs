//! A Luna stream on one thread: DTLS to the session server (`nxdtls`),
//! SPP over it, the setup of `main` and of every channel the server
//! announces, then the gamepad sent on `input` and the rest kept alive
//! (acks, time requests, RTCP, pongs). Display and audio messages are
//! counted only: decoding them comes later.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver as Inbox;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lunacore::proto::main::{
    client_message, server_message::Msg, ClientMessage, Pong, ServerMessage, TimeSyncRequest,
};
use lunacore::stream::framing::Frame;
use lunacore::stream::input::InputEvent;
use lunacore::stream::setup::{self, ClientIdentity};
use lunacore::stream::spp::{
    pt, rtcp_payload, time_reply, Clocks, ConnectForm, ConnectRequest, ConnectResponse, Event,
    Packet, Receiver, SendState, CONTROL_PAYLOAD, MAX_TX_PAYLOAD,
};
use lunacore::stream::Secret;
use nxdtls::{Client as Dtls, Pin, Secrets};
use prost::Message as _;

pub type Result<T> = std::result::Result<T, String>;

const STEP: Duration = Duration::from_secs(10);
const RTCP_EVERY: Duration = Duration::from_millis(100);
const TIMESYNC_EVERY: Duration = Duration::from_secs(10);
const REPORT_EVERY: Duration = Duration::from_secs(5);
/// Receive rate announced when less is measured (the Android client's
/// starting estimate).
const NOMINAL_RATE: u32 = 10 * 1024 * 1024;
const WANTED: [&str; 5] = ["display", "display2", "audio", "input", "participant"];

/// Where to connect and with what.
pub struct Target {
    pub ip: String,
    pub port: u16,
    pub pin: Pin,
    pub session_id: String,
    pub token: Secret,
}

/// Why the stream ended.
#[derive(Debug, PartialEq)]
pub enum End {
    /// Asked to stop (HOME, sleep).
    Stopped,
    /// The server closed it.
    Closed,
}

/// The DTLS link and SPP's counters.
struct Link {
    sock: UdpSocket,
    dtls: Dtls,
    send: SendState,
    recv: Receiver,
    started: Instant,
    start_secs: u32,
    bytes: u64,
    last_ts: u32,
    rate_mark: (u64, Instant),
    rtcp_sent: u64,
    /// Events read while waiting for another.
    held: Vec<Event>,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    unsafe { nxk_abi::user::syscall2(nxk_abi::sys::GETRANDOM, b.as_mut_ptr() as u64, N as u64) };
    b
}

impl Link {
    fn now_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    fn send(&mut self, channel: u8, kind: u8, msg: &[u8], max: usize) -> Result<()> {
        let now = self.now_us();
        for d in self.send.fragment(channel, kind, msg, max, now)? {
            let sealed = self.dtls.seal(&d).ok_or("DTLS: not connected")?;
            self.sock
                .send(&sealed)
                .map_err(|e| format!("send: {}", e))?;
        }
        Ok(())
    }

    fn send_frame(&mut self, channel: u8, frame: &Frame) -> Result<()> {
        self.send(channel, pt::DATA, &frame.encode(), MAX_TX_PAYLOAD)
    }

    /// Datagrams until `until` or an event: acks and time requests
    /// answered on the way.
    fn pump(&mut self, until: Instant) -> Result<Option<Event>> {
        if let Some(e) = self.recv.pop() {
            return Ok(Some(e));
        }
        let mut buf = [0u8; 2048];
        loop {
            let now = Instant::now();
            // A message held behind a missing one is released at its
            // deadline (the ordering budget).
            if self.recv.deadline().is_some_and(|d| now >= d) {
                self.recv.expire(now);
                if let Some(e) = self.recv.pop() {
                    return Ok(Some(e));
                }
            }
            if now >= until {
                self.recv.expire(now);
                return Ok(self.recv.pop());
            }
            let next = self.recv.deadline().map_or(until, |d| d.min(until));
            let wait = next
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            let _ = self
                .sock
                .set_read_timeout(Some(wait.min(Duration::from_millis(20))));
            let n = match self.sock.recv(&mut buf) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let (out, app) = self
                .dtls
                .on_datagram(Instant::now(), &buf[..n])
                .map_err(|e| format!("DTLS: {:?}", e))?;
            for d in out {
                let _ = self.sock.send(&d);
            }
            for d in app {
                let Ok(p) = Packet::decode(&d) else {
                    continue;
                };
                self.bytes += d.len() as u64;
                self.last_ts = p.header.timestamp;
                self.service(&p)?;
                if let Some(e) = self.recv.classify(&p) {
                    return Ok(Some(e));
                }
            }
        }
    }

    /// What a packet asks of us: an ack, a time reply.
    fn service(&mut self, p: &Packet) -> Result<()> {
        let h = &p.header;
        if h.force_ack && h.payload_type != pt::FEC {
            let mut ack = [0u8; 4];
            ack[..2].copy_from_slice(&u16::from(h.channel).to_le_bytes());
            ack[2..].copy_from_slice(&h.packet_seq.to_le_bytes());
            self.send(0, pt::ACK, &ack, CONTROL_PAYLOAD)?;
        }
        if matches!(h.payload_type, pt::TIME_REQUEST_32 | pt::TIME_REQUEST_64) {
            let clocks = Clocks {
                start_secs: self.start_secs,
                now_ms: self.started.elapsed().as_millis() as u64,
            };
            if let Some((kind, body)) = p
                .single_fragment_message()
                .and_then(|r| time_reply(h.payload_type, r, clocks))
            {
                self.send(0, kind, &body, CONTROL_PAYLOAD)?;
            }
        }
        Ok(())
    }

    /// The first event `want` takes within `STEP`; the others are kept.
    fn wait<T>(&mut self, what: &str, mut want: impl FnMut(&Event) -> Option<T>) -> Result<T> {
        if let Some(i) = self.held.iter().position(|e| want(e).is_some()) {
            let e = self.held.remove(i);
            return Ok(want(&e).expect("matched"));
        }
        let until = Instant::now() + STEP;
        while Instant::now() < until {
            if let Some(e) = self.pump(until)? {
                if matches!(e, Event::Closed) {
                    return Err(format!("closed by the server while waiting for {}", what));
                }
                match want(&e) {
                    Some(t) => return Ok(t),
                    None => self.held.push(e),
                }
            }
        }
        let held: Vec<String> = self
            .held
            .iter()
            .map(|e| match e {
                Event::Message(m) => format!("msg@{}", m.channel),
                Event::ChannelConfirmed { channel, .. } => format!("confirm@{}", channel),
                other => format!("{:?}", other),
            })
            .collect();
        Err(format!("no {} within {:?} (held: {:?})", what, STEP, held))
    }

    fn rtcp(&mut self) -> Result<()> {
        self.rtcp_sent += 1;
        if self.rtcp_sent % 10 == 1 {
            let now = self.now_us();
            let mut req = [0u8; 18];
            req[2..10].copy_from_slice(&now.to_le_bytes());
            req[10..].copy_from_slice(&(now / 1000).to_le_bytes());
            self.send(0, pt::TIME_REQUEST_64, &req, CONTROL_PAYLOAD)?;
        }
        let (before, then) = self.rate_mark;
        let secs = then.elapsed().as_secs_f64().max(0.001);
        let rate = ((self.bytes - before) as f64 * 8.0 / secs) as u64;
        self.rate_mark = (self.bytes, Instant::now());
        let rate = u32::try_from(rate.max(NOMINAL_RATE as u64)).unwrap_or(u32::MAX);
        self.send(
            0,
            pt::RTCP,
            &rtcp_payload(rate, self.last_ts),
            CONTROL_PAYLOAD,
        )
    }
}

/// DTLS and SPP's connect: a link the session can use.
fn connect(t: &Target) -> Result<Link> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("UDP: {}", e))?;
    sock.connect((t.ip.as_str(), t.port))
        .map_err(|e| format!("UDP: {}", e))?;
    let mut dtls = Dtls::new(
        t.pin.clone(),
        Secrets {
            random: random(),
            ecdh: random(),
        },
    );
    for d in dtls.start(Instant::now()) {
        sock.send(&d).map_err(|e| format!("send: {}", e))?;
    }
    let until = Instant::now() + STEP;
    let mut buf = [0u8; 2048];
    while !dtls.is_connected() {
        if Instant::now() >= until {
            return Err("DTLS: no handshake within 10 s".into());
        }
        let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
        match sock.recv(&mut buf) {
            Ok(n) => {
                let (out, _) = dtls
                    .on_datagram(Instant::now(), &buf[..n])
                    .map_err(|e| format!("DTLS: {:?}", e))?;
                for d in out {
                    let _ = sock.send(&d);
                }
            }
            Err(_) => {
                for d in dtls.on_timer(Instant::now()) {
                    let _ = sock.send(&d);
                }
            }
        }
    }
    println!("luna: DTLS up");
    let start_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);
    let mut link = Link {
        sock,
        dtls,
        send: SendState::new(true),
        recv: Receiver::default(),
        started: Instant::now(),
        start_secs,
        bytes: 0,
        last_ts: 0,
        rate_mark: (0, Instant::now()),
        rtcp_sent: 0,
        held: Vec::new(),
    };
    let id = u32::from_le_bytes(random());
    let req = ConnectRequest::new(ConnectForm::Full, id).encode();
    link.send(0, pt::CONN_REQUEST, &req, CONTROL_PAYLOAD)?;
    // The response is a control packet: read it before classification.
    let until = Instant::now() + STEP;
    let mut buf = [0u8; 2048];
    loop {
        if Instant::now() >= until {
            return Err("SPP: no connect response".into());
        }
        let _ = link.sock.set_read_timeout(Some(Duration::from_millis(50)));
        let Ok(n) = link.sock.recv(&mut buf) else {
            continue;
        };
        let (out, app) = link
            .dtls
            .on_datagram(Instant::now(), &buf[..n])
            .map_err(|e| format!("DTLS: {:?}", e))?;
        for d in out {
            let _ = link.sock.send(&d);
        }
        for d in app {
            let Ok(p) = Packet::decode(&d) else {
                continue;
            };
            link.service(&p)?;
            if p.header.payload_type == pt::CONN_RESPONSE {
                let r = p
                    .single_fragment_message()
                    .and_then(|m| ConnectResponse::decode(m).ok())
                    .ok_or("SPP: bad connect response")?;
                if !r.accepted {
                    return Err(format!(
                        "SPP: refused (server version {})",
                        r.server_version
                    ));
                }
                println!("luna: SPP connected (server version {})", r.server_version);
                return Ok(link);
            }
        }
    }
}

/// The session's channels by id, their names.
struct Session {
    link: Link,
    names: HashMap<u8, String>,
    next: u8,
}

impl Session {
    /// Asks for channel `name` and waits for its SPP confirmation.
    fn open(&mut self, name: &str) -> Result<u8> {
        let id = self.next;
        if id > lunacore::stream::spp::MAX_CHANNEL {
            return Err("all channels in use".into());
        }
        self.next += 1;
        let payload = setup::channel_request_payload(setup::spp_channel_name(name), true)
            .map_err(|e| e.to_string())?;
        self.link
            .send(id, pt::CHANNEL_REQUEST, &payload, CONTROL_PAYLOAD)?;
        self.link
            .wait(&format!("channel {} confirmed", name), |e| match e {
                Event::ChannelConfirmed { channel, .. } if *channel == id => Some(()),
                _ => None,
            })?;
        self.names.insert(id, name.to_string());
        Ok(id)
    }

    /// The next frame on channel `id`.
    fn frame(&mut self, id: u8, what: &str) -> Result<Frame> {
        let bytes = self.link.wait(what, |e| match e {
            Event::Message(m) if m.channel == id => Some(m.bytes.clone()),
            _ => None,
        })?;
        Frame::decode(&bytes).map_err(|e| e.to_string())
    }
}

fn main_message(frame: &Frame) -> Option<Msg> {
    ServerMessage::decode(frame.message.as_slice()).ok()?.msg
}

/// Connects, sets the session up, then runs until `stop` or the server
/// closes it; gamepad events come on `input`.
pub fn run(t: &Target, input: &Inbox<InputEvent>, stop: &AtomicBool) -> Result<End> {
    let link = connect(t)?;
    let mut s = Session {
        link,
        names: HashMap::new(),
        next: 1,
    };
    let main = s.open("main")?;
    let identity = ClientIdentity {
        os: "linux".into(),
        ..ClientIdentity::default()
    };
    let req = setup::connection_request(&t.session_id, &t.token, &identity);
    s.link.send_frame(main, &req)?;
    let answer = s.frame(main, "the connection confirmation")?;
    let confirm = setup::parse_connection_answer(&answer).map_err(|e| e.to_string())?;
    let connection_id = confirm.connection_id.ok_or("no connection id")?;
    println!("luna: session connected");
    // The channel list (pings may come first).
    let announced = loop {
        let f = s.frame(main, "the channel list")?;
        match main_message(&f) {
            Some(Msg::ChannelNotification(n)) => break n.channels,
            Some(Msg::Ping(p)) => pong(&mut s.link, main, p.ping_timestamp)?,
            _ => {}
        }
    };
    let names: Vec<&str> = announced.iter().filter_map(|c| c.name.as_deref()).collect();
    println!("luna: announced {:?}", names);
    let mut requested = Vec::new();
    for c in announced {
        let (Some(name), token) = (c.name, c.authentication_token) else {
            continue;
        };
        if !WANTED.contains(&name.as_str()) || s.names.values().any(|n| *n == name) {
            continue;
        }
        let id = s.open(&name)?;
        println!("luna: channel {} is {}", name, id);
        if setup::needs_setup(&name) {
            let req = setup::channel_connection_request(
                &t.session_id,
                connection_id,
                &name,
                &Secret::new(token.unwrap_or_default()),
            );
            s.link.send_frame(id, &req)?;
            requested.push((id, name));
        }
    }
    for (id, name) in &requested {
        let f = s.frame(*id, &format!("channel {} set up", name))?;
        setup::parse_channel_answer(&f).map_err(|e| format!("{}: {}", name, e))?;
    }
    let list: Vec<&str> = s.names.values().map(String::as_str).collect();
    println!("luna: channels {:?}", list);
    let input_id = s.names.iter().find(|(_, n)| *n == "input").map(|(i, _)| *i);
    streaming(&mut s, main, input_id, input, stop)
}

fn pong(link: &mut Link, main: u8, ping: Option<i64>) -> Result<()> {
    let m = ClientMessage {
        msg: Some(client_message::Msg::Pong(Pong {
            ping_timestamp: ping,
            pong_timestamp: Some(link.now_us() as i64),
        })),
    };
    link.send_frame(
        main,
        &Frame {
            message: m.encode_to_vec(),
            raw: Vec::new(),
        },
    )
}

fn streaming(
    s: &mut Session,
    main: u8,
    input_id: Option<u8>,
    input: &Inbox<InputEvent>,
    stop: &AtomicBool,
) -> Result<End> {
    let mut counts: HashMap<u8, (u64, u64)> = HashMap::new();
    let (mut next_rtcp, mut next_sync, mut next_report) = (
        Instant::now(),
        Instant::now() + TIMESYNC_EVERY,
        Instant::now() + REPORT_EVERY,
    );
    let mut sync_id = 0u32;
    let mut held = std::mem::take(&mut s.link.held);
    let end = loop {
        if stop.load(Ordering::Relaxed) {
            break End::Stopped;
        }
        let event = match held.pop() {
            Some(e) => Some(e),
            None => s
                .link
                .pump(next_rtcp.min(Instant::now() + Duration::from_millis(10)))?,
        };
        match event {
            Some(Event::Message(m)) if m.channel == main => {
                if let Ok(f) = Frame::decode(&m.bytes) {
                    match main_message(&f) {
                        Some(Msg::Ping(p)) => pong(&mut s.link, main, p.ping_timestamp)?,
                        Some(Msg::ConnectionClose(c)) => {
                            println!("luna: the server ends the session (reason {:?})", c.reason);
                            break End::Closed;
                        }
                        _ => {}
                    }
                }
            }
            Some(Event::Message(m)) => {
                let c = counts.entry(m.channel).or_default();
                (c.0, c.1) = (c.0 + 1, c.1 + m.bytes.len() as u64);
            }
            Some(Event::Closed) => break End::Closed,
            _ => {}
        }
        if let Some(id) = input_id {
            while let Ok(ev) = input.try_recv() {
                let f = Frame {
                    message: ev.encode(),
                    raw: Vec::new(),
                };
                s.link.send_frame(id, &f)?;
            }
        }
        let now = Instant::now();
        if now >= next_rtcp {
            s.link.rtcp()?;
            next_rtcp = now + RTCP_EVERY;
        }
        if now >= next_sync {
            sync_id = sync_id.wrapping_add(1);
            let m = ClientMessage {
                msg: Some(client_message::Msg::TimesyncRequest(TimeSyncRequest {
                    id: Some(sync_id),
                    client_timestamp: Some(s.link.now_us()),
                })),
            };
            s.link.send_frame(
                main,
                &Frame {
                    message: m.encode_to_vec(),
                    raw: Vec::new(),
                },
            )?;
            next_sync = now + TIMESYNC_EVERY;
        }
        if now >= next_report {
            let mut line = String::from("luna: received");
            for (id, (n, bytes)) in &counts {
                let name = s.names.get(id).map_or("?", String::as_str);
                line.push_str(&format!(" {} {} msgs {} KB,", name, n, bytes / 1024));
            }
            println!("{}", line.trim_end_matches(','));
            next_report = now + REPORT_EVERY;
        }
    };
    let _ = s.link.send(0, pt::CONN_CLOSE, &[], CONTROL_PAYLOAD);
    if let Some(d) = s.link.dtls.close() {
        let _ = s.link.sock.send(&d);
    }
    Ok(end)
}
