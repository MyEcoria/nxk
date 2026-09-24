//! TERMINAL page: the USB-C console's output (the console ring, with the
//! echo of what the PC typed) read as a terminal would: CR, LF,
//! backspace, SGR colors, and the shell's marks (OSC 7 directory, OSC
//! 133 prompt, command, status) to draw its prompts in a zsh look.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libnx::time;

use crate::screen::{BAD, BLUE, DIM, FG, GOOD};

/// Lines and commands kept.
const KEEP: usize = 400;
/// Longest line kept, in characters.
const MAX_LINE: usize = 512;
/// Longest escape sequence kept (longer ones are dropped).
const MAX_ESC: usize = 512;

pub struct Cmd {
    /// As typed, a `wifi connect` passphrase masked.
    pub line: String,
    /// Directory it ran in.
    cwd: String,
    /// What it failed with: errno name or exit code.
    pub error: Option<String>,
    /// Wall-clock start, ns since 1970.
    at: u64,
}

impl Cmd {
    /// Start time, `hh:mm:ss` UTC.
    pub fn time(&self) -> String {
        let s = self.at / 1_000_000_000 % 86_400;
        format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }
}

enum Entry {
    Out(Vec<(String, u32)>),
    Cmd(Cmd),
}

/// One screen row: colored text, a right-aligned part, the cursor after the text.
pub struct Row {
    pub segs: Vec<(String, u32)>,
    pub right: Vec<(String, u32)>,
    pub cursor: bool,
}

#[derive(PartialEq)]
enum Mode {
    Out,
    /// Between OSC 133 A and B: the shell's own prompt, not shown.
    Prompt,
    /// Between B and C: the echo of the command being typed.
    Typing,
    /// Between C and D: the command's output.
    Running,
}

/// Escape sequence being read.
enum Seq {
    Text,
    /// After ESC.
    Start,
    Csi(Vec<u8>),
    Osc(Vec<u8>),
    /// ESC inside an OSC: `ESC \` ends it.
    OscEnd(Vec<u8>),
}

pub struct Term {
    entries: VecDeque<Entry>,
    /// Line being written and the column written next.
    line: Vec<(char, u32)>,
    col: usize,
    /// Command being typed and its column.
    typed: Vec<char>,
    tcol: usize,
    /// A line ended while at the prompt: the command if OSC 133 C
    /// follows, else output that came meanwhile (a service's log).
    pending: Option<String>,
    color: u32,
    mode: Mode,
    esc: Seq,
    cwd: String,
    /// Bytes of an unfinished UTF-8 character.
    utf8: Vec<u8>,
}

/// Write `v` at `col` (overwriting, as a terminal does) and move on.
fn put<T>(buf: &mut Vec<T>, col: &mut usize, v: T) {
    if *col < buf.len() {
        buf[*col] = v;
    } else if buf.len() < MAX_LINE {
        buf.push(v);
    }
    *col += 1;
}

/// `wifi connect SSID PASSPHRASE`: the passphrase never reaches the screen.
fn mask(line: &str) -> String {
    let w: Vec<&str> = line.split_whitespace().collect();
    if w.len() >= 4 && w[0] == "wifi" && w[1] == "connect" {
        format!("{} {} {} ********", w[0], w[1], w[2])
    } else {
        line.into()
    }
}

fn prompt(cwd: &str) -> Vec<(String, u32)> {
    vec![
        ("pc@nxk".into(), GOOD),
        (" ".into(), FG),
        (cwd.into(), BLUE),
        (" % ".into(), FG),
    ]
}

/// xterm 256-color palette entry, A B G R.
fn xterm(n: u8) -> u32 {
    const BASIC: [u32; 16] = [
        0x000000, 0xCD0000, 0x00CD00, 0xCDCD00, 0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5, 0x7F7F7F,
        0xFF0000, 0x00FF00, 0xFFFF00, 0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
    ];
    let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
    let (r, g, b) = match n {
        0..=15 => {
            let c = BASIC[n as usize];
            (c >> 16, c >> 8 & 0xFF, c & 0xFF)
        }
        16..=231 => {
            let k = n - 16;
            (level(k / 36), level(k / 6 % 6), level(k % 6))
        }
        _ => {
            let v = 8 + 10 * (n - 232) as u32;
            (v, v, v)
        }
    };
    0xFF00_0000 | b << 16 | g << 8 | r
}

/// Rows of `segs` wrapped at `cols`; `right` on the first row if it fits.
fn wrap(
    rows: &mut Vec<Row>,
    cols: usize,
    segs: Vec<(String, u32)>,
    right: Vec<(String, u32)>,
    cursor: bool,
) {
    let first = rows.len();
    let (mut row, mut n) = (Vec::new(), 0);
    for (t, c) in segs {
        let mut rest = t.as_str();
        while !rest.is_empty() {
            if n == cols {
                rows.push(Row {
                    segs: core::mem::take(&mut row),
                    right: Vec::new(),
                    cursor: false,
                });
                n = 0;
            }
            let k = (cols - n).min(rest.chars().count());
            let cut = rest.char_indices().nth(k).map_or(rest.len(), |(i, _)| i);
            row.push((rest[..cut].into(), c));
            n += k;
            rest = &rest[cut..];
        }
    }
    rows.push(Row {
        segs: row,
        right: Vec::new(),
        cursor,
    });
    // zsh drops the right prompt when it does not fit.
    let len = |s: &[(String, u32)]| s.iter().map(|(t, _)| t.chars().count()).sum::<usize>();
    if !right.is_empty() && len(&rows[first].segs) + 2 + len(&right) <= cols {
        rows[first].right = right;
    }
}

impl Term {
    pub fn new() -> Self {
        Self {
            // Full size at once: a buffer past the allocator's small
            // blocks would be mapped anew as it grows.
            entries: VecDeque::with_capacity(KEEP + 1),
            line: Vec::with_capacity(MAX_LINE),
            col: 0,
            typed: Vec::with_capacity(MAX_LINE),
            tcol: 0,
            pending: None,
            color: FG,
            mode: Mode::Out,
            esc: Seq::Text,
            cwd: "/".into(),
            utf8: Vec::new(),
        }
    }

    /// Console output, as it comes.
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match core::mem::replace(&mut self.esc, Seq::Text) {
                Seq::Text => self.byte(b),
                Seq::Start => match b {
                    b'[' => self.esc = Seq::Csi(Vec::new()),
                    b']' => self.esc = Seq::Osc(Vec::new()),
                    _ => {}
                },
                Seq::Csi(mut p) => {
                    if (0x40..=0x7E).contains(&b) {
                        if b == b'm' {
                            self.sgr(&p);
                        }
                    } else if p.len() < MAX_ESC {
                        p.push(b);
                        self.esc = Seq::Csi(p);
                    }
                }
                Seq::Osc(mut p) => match b {
                    0x07 => self.osc(&p),
                    0x1B => self.esc = Seq::OscEnd(p),
                    _ => {
                        if p.len() < MAX_ESC {
                            p.push(b);
                        }
                        self.esc = Seq::Osc(p);
                    }
                },
                Seq::OscEnd(p) => {
                    self.osc(&p);
                    if b != b'\\' {
                        self.byte(b);
                    }
                }
            }
        }
    }

    fn byte(&mut self, b: u8) {
        if b == 0x1B {
            self.esc = Seq::Start;
            return;
        }
        if b < 0x80 {
            self.utf8.clear();
            self.ch(b as char);
            return;
        }
        self.utf8.push(b);
        match core::str::from_utf8(&self.utf8) {
            Ok(s) => {
                let c = s.chars().next().unwrap_or('?');
                self.utf8.clear();
                self.ch(c);
            }
            Err(e) if e.error_len().is_none() && self.utf8.len() < 4 => {}
            Err(_) => {
                self.utf8.clear();
                self.ch('?');
            }
        }
    }

    fn ch(&mut self, c: char) {
        let typing = self.mode == Mode::Typing;
        let col = if typing {
            &mut self.tcol
        } else {
            &mut self.col
        };
        match c {
            '\n' if typing => self.line_typed(),
            '\n' => self.commit(),
            '\r' => *col = 0,
            '\x08' => *col = col.saturating_sub(1),
            c if (c as u32) < 0x20 || c == '\x7F' => {}
            _ if self.mode == Mode::Prompt => {}
            c if typing => put(&mut self.typed, col, c),
            c => put(&mut self.line, col, (c, self.color)),
        }
    }

    /// The line being written becomes an entry.
    fn commit(&mut self) {
        let mut segs: Vec<(String, u32)> = Vec::new();
        let end = self
            .line
            .iter()
            .rposition(|&(c, _)| c != ' ')
            .map_or(0, |i| i + 1);
        for &(c, color) in &self.line[..end] {
            match segs.last_mut() {
                Some((t, k)) if *k == color => t.push(c),
                _ => segs.push((String::from(c), color)),
            }
        }
        self.push(Entry::Out(segs));
        self.line.clear();
        self.col = 0;
    }

    fn push(&mut self, e: Entry) {
        self.entries.push_back(e);
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
    }

    fn typed_line(&self) -> String {
        let s: String = self.typed.iter().collect();
        mask(s.trim())
    }

    /// A line ended at the prompt; the one before was output.
    fn line_typed(&mut self) {
        let line = self.typed_line();
        if let Some(p) = self.pending.replace(line) {
            self.push(Entry::Out(vec![(p, FG)]));
        }
        self.typed.clear();
        self.tcol = 0;
    }

    fn sgr(&mut self, p: &[u8]) {
        let s = core::str::from_utf8(p).unwrap_or("");
        let n: Vec<u32> = s.split(';').map(|x| x.parse().unwrap_or(0)).collect();
        let mut i = 0;
        while i < n.len() {
            match n[i] {
                0 | 39 => self.color = FG,
                k @ 30..=37 => self.color = xterm((k - 30) as u8),
                k @ 90..=97 => self.color = xterm((k - 90 + 8) as u8),
                38 if n.get(i + 1) == Some(&5) => {
                    self.color = xterm(n.get(i + 2).copied().unwrap_or(7) as u8);
                    i += 2;
                }
                38 if n.get(i + 1) == Some(&2) && i + 4 < n.len() => {
                    let (r, g, b) = (n[i + 2] & 0xFF, n[i + 3] & 0xFF, n[i + 4] & 0xFF);
                    self.color = 0xFF00_0000 | b << 16 | g << 8 | r;
                    i += 4;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn osc(&mut self, p: &[u8]) {
        let s = core::str::from_utf8(p).unwrap_or("");
        if let Some(url) = s.strip_prefix("7;file://") {
            // Host, then the path.
            self.cwd = url.find('/').map_or("/", |i| &url[i..]).into();
            return;
        }
        let Some(mark) = s.strip_prefix("133;") else {
            return;
        };
        let mut parts = mark.split(';');
        match parts.next() {
            Some("A") => {
                match self.pending.take() {
                    // Enter on an empty line: the prompt stays, like zsh.
                    Some(p) if p.is_empty() => {
                        let cwd = self.cwd.clone();
                        let e = Cmd {
                            line: p,
                            cwd,
                            error: None,
                            at: time::realtime_ns(),
                        };
                        self.push(Entry::Cmd(e));
                    }
                    Some(p) => self.push(Entry::Out(vec![(p, FG)])),
                    None => {}
                }
                if !self.line.is_empty() {
                    self.commit();
                }
                self.mode = Mode::Prompt;
            }
            Some("B") => {
                self.typed.clear();
                self.tcol = 0;
                self.pending = None;
                self.mode = Mode::Typing;
            }
            Some("C") => {
                let line = self.pending.take().unwrap_or_else(|| self.typed_line());
                let cwd = self.cwd.clone();
                self.push(Entry::Cmd(Cmd {
                    line,
                    cwd,
                    error: None,
                    at: time::realtime_ns(),
                }));
                self.color = FG;
                self.mode = Mode::Running;
            }
            Some("D") => {
                let code = parts
                    .next()
                    .and_then(|c| c.parse::<i64>().ok())
                    .unwrap_or(0);
                let name = parts.find_map(|x| x.strip_prefix("err="));
                if code != 0 {
                    if let Some(Entry::Cmd(c)) = self
                        .entries
                        .iter_mut()
                        .rev()
                        .find(|e| matches!(e, Entry::Cmd(_)))
                    {
                        c.error = Some(name.map_or_else(|| format!("exit {}", code), String::from));
                    }
                }
                if !self.line.is_empty() {
                    self.commit();
                }
                self.mode = Mode::Out;
            }
            _ => {}
        }
    }

    /// The last `n` commands typed, oldest first (empty lines left out).
    pub fn commands(&self, n: usize) -> Vec<&Cmd> {
        let mut v: Vec<&Cmd> = self
            .entries
            .iter()
            .rev()
            .filter_map(|e| match e {
                Entry::Cmd(c) if !c.line.is_empty() => Some(c),
                _ => None,
            })
            .take(n)
            .collect();
        v.reverse();
        v
    }

    /// The last `max` rows `cols` wide; the last one is the live prompt
    /// or line with the cursor, or `notice` (terminal closed) instead.
    pub fn rows(&self, cols: usize, max: usize, notice: Option<String>) -> Vec<Row> {
        let mut rows = Vec::new();
        if cols == 0 {
            return rows;
        }
        // Entries from the newest back, until the screen is full.
        let mut first = self.entries.len();
        let mut count = 0;
        while first > 0 && count < max {
            first -= 1;
            count += match &self.entries[first] {
                Entry::Out(segs) => segs.iter().map(|(t, _)| t.chars().count()).sum::<usize>(),
                Entry::Cmd(c) => c.cwd.len() + c.line.len() + 10,
            }
            .div_ceil(cols)
            .max(1);
        }
        for e in self.entries.range(first..) {
            match e {
                Entry::Out(segs) => wrap(&mut rows, cols, segs.clone(), Vec::new(), false),
                Entry::Cmd(c) => {
                    let mut segs = prompt(&c.cwd);
                    segs.push((c.line.clone(), FG));
                    let right = match (&c.error, c.line.is_empty()) {
                        (_, true) => Vec::new(),
                        (Some(e), _) => vec![(format!("{}  ", e), BAD), (c.time(), DIM)],
                        (None, _) => vec![(c.time(), DIM)],
                    };
                    wrap(&mut rows, cols, segs, right, false);
                }
            }
        }
        let partial: Vec<(String, u32)> = self
            .line
            .iter()
            .map(|&(c, k)| (String::from(c), k))
            .collect();
        let at_prompt = matches!(self.mode, Mode::Prompt | Mode::Typing);
        match notice {
            Some(t) => {
                if !partial.is_empty() {
                    wrap(&mut rows, cols, partial, Vec::new(), false);
                }
                wrap(&mut rows, cols, vec![(t, DIM)], Vec::new(), false);
            }
            None if at_prompt => {
                if let Some(p) = self.pending.as_ref().filter(|p| !p.is_empty()) {
                    wrap(&mut rows, cols, vec![(p.clone(), FG)], Vec::new(), false);
                }
                let mut segs = prompt(&self.cwd);
                segs.push((self.typed_line(), FG));
                wrap(&mut rows, cols, segs, Vec::new(), true);
            }
            None => wrap(&mut rows, cols, partial, Vec::new(), true),
        }
        let drop = rows.len().saturating_sub(max);
        rows.drain(..drop);
        rows
    }
}
