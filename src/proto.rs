//! Wire protocol (DESIGN §5.1): length-prefixed frames, identical on the ssh
//! leg and on the master's unix socket, plus the plain-text marker lines that
//! precede the first frame (DESIGN §3, §8).
//!
//! ```text
//! +---------+------------+----------------+
//! | type u8 | len u32 BE | payload[len]   |
//! +---------+------------+----------------+
//! ```

use std::fmt;

/// Bumped whenever a frame's layout or meaning changes.
pub const PROTO_VERSION: u16 = 1;

/// Largest payload accepted. Senders chunk DATA/INPUT well below this.
pub const MAX_FRAME: usize = 1 << 20;

/// Largest DATA/INPUT chunk a sender puts in one frame.
pub const MAX_CHUNK: usize = 32 * 1024;

const HEADER: usize = 5;

/// Written by the remote side immediately before its first frame.
pub const READY_MARKER: &str = "ACS-READY";
/// Written by the remote prelude when no usable binary is installed.
pub const NEED_MARKER: &str = "ACS-NEED";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// How a HELLO wants to find its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Attach,
    Create,
    AttachOrCreate,
}

/// Where a client wants output to resume from after a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    pub instance: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub proto: u16,
    pub session: String,
    pub mode: Mode,
    /// Display identity, `user@host` of the client (DESIGN §4.5).
    pub identity: String,
    /// Take over even when another identity is attached.
    pub force: bool,
    pub term: String,
    pub colorterm: String,
    pub size: WinSize,
    pub resume: Option<Resume>,
    /// Command to run when the session is created; empty means `$SHELL -l`.
    pub command: Vec<String>,
}

/// How the master is serving this attach (DESIGN §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// New client or unknown terminal state: clear and redraw.
    Fresh,
    /// Output continues exactly where the client left off.
    Resumed,
    /// The requested offset was overwritten: clear and redraw.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub proto: u16,
    /// The session's name (the client may not know it: `--new`).
    pub session: String,
    pub instance: u64,
    /// Offset of the next byte the master will send.
    pub offset: u64,
    pub created: bool,
    pub kind: AttachKind,
    /// Input bytes the master has written to the pty so far: the sequence
    /// number of the next INPUT byte it will accept.
    pub input_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusInfo {
    pub name: String,
    pub attached: bool,
    /// Identity attached now, or last attached; empty if never.
    pub identity: String,
    pub creator: String,
    /// Unix seconds.
    pub created_at: u64,
    pub idle_secs: u64,
    pub command: String,
    pub size: WinSize,
    pub version: String,
    pub pid: u32,
}

/// Error codes carried by [`Msg::Error`].
pub mod err {
    pub const PROTO_MISMATCH: u16 = 1;
    pub const NO_SESSION: u16 = 2;
    pub const INTERNAL: u16 = 3;
    pub const BAD_REQUEST: u16 = 4;
    pub const EXISTS: u16 = 5;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Hello(Hello),
    Welcome(Welcome),
    /// Another identity is attached (DESIGN §4.5).
    Busy {
        identity: String,
        since: u64,
    },
    Data {
        offset: u64,
        bytes: Vec<u8>,
    },
    Input {
        seq: u64,
        bytes: Vec<u8>,
    },
    Ack {
        seq: u64,
    },
    Resize(WinSize),
    Ping(u64),
    Pong(u64),
    Detach,
    Kill,
    /// Child wait status as returned by `waitpid`.
    Exit {
        status: i32,
    },
    Takeover,
    Status,
    StatusReply(StatusInfo),
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    TooLarge(usize),
    UnknownType(u8),
    Truncated(&'static str),
    BadUtf8,
    BadValue(&'static str),
    Trailing(u8),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the limit"),
            ProtoError::UnknownType(t) => write!(f, "unknown frame type {t}"),
            ProtoError::Truncated(what) => write!(f, "truncated frame reading {what}"),
            ProtoError::BadUtf8 => write!(f, "string is not UTF-8"),
            ProtoError::BadValue(what) => write!(f, "invalid value for {what}"),
            ProtoError::Trailing(t) => write!(f, "trailing bytes in frame type {t}"),
        }
    }
}

impl std::error::Error for ProtoError {}

mod ty {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const BUSY: u8 = 3;
    pub const DATA: u8 = 4;
    pub const INPUT: u8 = 5;
    pub const ACK: u8 = 6;
    pub const RESIZE: u8 = 7;
    pub const PING: u8 = 8;
    pub const PONG: u8 = 9;
    pub const DETACH: u8 = 10;
    pub const KILL: u8 = 11;
    pub const EXIT: u8 = 12;
    pub const TAKEOVER: u8 = 13;
    pub const STATUS: u8 = 14;
    pub const STATUS_REPLY: u8 = 15;
    pub const ERROR: u8 = 16;
}

struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn size(&mut self, s: &WinSize) {
        self.u16(s.cols);
        self.u16(s.rows);
        self.u16(s.xpixel);
        self.u16(s.ypixel);
    }
}

struct R<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ProtoError> {
        if self.buf.len() - self.pos < n {
            return Err(ProtoError::Truncated(what));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, ProtoError> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(self.take(2, what)?.try_into().unwrap()))
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn u64(&mut self, what: &'static str) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(self.take(8, what)?.try_into().unwrap()))
    }
    fn i32(&mut self, what: &'static str) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn bytes(&mut self, what: &'static str) -> Result<Vec<u8>, ProtoError> {
        let n = self.u32(what)? as usize;
        Ok(self.take(n, what)?.to_vec())
    }
    fn str(&mut self, what: &'static str) -> Result<String, ProtoError> {
        String::from_utf8(self.bytes(what)?).map_err(|_| ProtoError::BadUtf8)
    }
    fn bool(&mut self, what: &'static str) -> Result<bool, ProtoError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtoError::BadValue(what)),
        }
    }
    fn size(&mut self) -> Result<WinSize, ProtoError> {
        Ok(WinSize {
            cols: self.u16("cols")?,
            rows: self.u16("rows")?,
            xpixel: self.u16("xpixel")?,
            ypixel: self.u16("ypixel")?,
        })
    }
    fn done(&self, t: u8) -> Result<(), ProtoError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ProtoError::Trailing(t))
        }
    }
}

impl Msg {
    /// Append this message as one frame to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0; HEADER]);
        let t = {
            let mut w = W(out);
            match self {
                Msg::Hello(h) => {
                    w.u16(h.proto);
                    w.str(&h.session);
                    w.u8(match h.mode {
                        Mode::Attach => 0,
                        Mode::Create => 1,
                        Mode::AttachOrCreate => 2,
                    });
                    w.str(&h.identity);
                    w.u8(h.force as u8);
                    w.str(&h.term);
                    w.str(&h.colorterm);
                    w.size(&h.size);
                    match h.resume {
                        None => w.u8(0),
                        Some(r) => {
                            w.u8(1);
                            w.u64(r.instance);
                            w.u64(r.offset);
                        }
                    }
                    w.u32(h.command.len() as u32);
                    for a in &h.command {
                        w.str(a);
                    }
                    ty::HELLO
                }
                Msg::Welcome(x) => {
                    w.u16(x.proto);
                    w.str(&x.session);
                    w.u64(x.instance);
                    w.u64(x.offset);
                    w.u8(x.created as u8);
                    w.u8(match x.kind {
                        AttachKind::Fresh => 0,
                        AttachKind::Resumed => 1,
                        AttachKind::Gap => 2,
                    });
                    w.u64(x.input_seq);
                    ty::WELCOME
                }
                Msg::Busy { identity, since } => {
                    w.str(identity);
                    w.u64(*since);
                    ty::BUSY
                }
                Msg::Data { offset, bytes } => {
                    w.u64(*offset);
                    w.0.extend_from_slice(bytes);
                    ty::DATA
                }
                Msg::Input { seq, bytes } => {
                    w.u64(*seq);
                    w.0.extend_from_slice(bytes);
                    ty::INPUT
                }
                Msg::Ack { seq } => {
                    w.u64(*seq);
                    ty::ACK
                }
                Msg::Resize(s) => {
                    w.size(s);
                    ty::RESIZE
                }
                Msg::Ping(n) => {
                    w.u64(*n);
                    ty::PING
                }
                Msg::Pong(n) => {
                    w.u64(*n);
                    ty::PONG
                }
                Msg::Detach => ty::DETACH,
                Msg::Kill => ty::KILL,
                Msg::Exit { status } => {
                    w.i32(*status);
                    ty::EXIT
                }
                Msg::Takeover => ty::TAKEOVER,
                Msg::Status => ty::STATUS,
                Msg::StatusReply(s) => {
                    w.str(&s.name);
                    w.u8(s.attached as u8);
                    w.str(&s.identity);
                    w.str(&s.creator);
                    w.u64(s.created_at);
                    w.u64(s.idle_secs);
                    w.str(&s.command);
                    w.size(&s.size);
                    w.str(&s.version);
                    w.u32(s.pid);
                    ty::STATUS_REPLY
                }
                Msg::Error { code, message } => {
                    w.u16(*code);
                    w.str(message);
                    ty::ERROR
                }
            }
        };
        let len = (out.len() - start - HEADER) as u32;
        out[start] = t;
        out[start + 1..start + HEADER].copy_from_slice(&len.to_be_bytes());
    }

    /// Encode into a fresh buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }

    fn decode(t: u8, p: &[u8]) -> Result<Msg, ProtoError> {
        let mut r = R { buf: p, pos: 0 };
        let m = match t {
            ty::HELLO => {
                let proto = r.u16("proto")?;
                let session = r.str("session")?;
                let mode = match r.u8("mode")? {
                    0 => Mode::Attach,
                    1 => Mode::Create,
                    2 => Mode::AttachOrCreate,
                    _ => return Err(ProtoError::BadValue("mode")),
                };
                let identity = r.str("identity")?;
                let force = r.bool("force")?;
                let term = r.str("term")?;
                let colorterm = r.str("colorterm")?;
                let size = r.size()?;
                let resume = if r.bool("resume")? {
                    Some(Resume {
                        instance: r.u64("instance")?,
                        offset: r.u64("offset")?,
                    })
                } else {
                    None
                };
                let n = r.u32("argc")? as usize;
                if n > 4096 {
                    return Err(ProtoError::BadValue("argc"));
                }
                let mut command = Vec::with_capacity(n);
                for _ in 0..n {
                    command.push(r.str("argv")?);
                }
                Msg::Hello(Hello {
                    proto,
                    session,
                    mode,
                    identity,
                    force,
                    term,
                    colorterm,
                    size,
                    resume,
                    command,
                })
            }
            ty::WELCOME => Msg::Welcome(Welcome {
                proto: r.u16("proto")?,
                session: r.str("session")?,
                instance: r.u64("instance")?,
                offset: r.u64("offset")?,
                created: r.bool("created")?,
                kind: match r.u8("kind")? {
                    0 => AttachKind::Fresh,
                    1 => AttachKind::Resumed,
                    2 => AttachKind::Gap,
                    _ => return Err(ProtoError::BadValue("kind")),
                },
                input_seq: r.u64("input_seq")?,
            }),
            ty::BUSY => Msg::Busy {
                identity: r.str("identity")?,
                since: r.u64("since")?,
            },
            ty::DATA => {
                let offset = r.u64("offset")?;
                let bytes = p[r.pos..].to_vec();
                r.pos = p.len();
                Msg::Data { offset, bytes }
            }
            ty::INPUT => {
                let seq = r.u64("seq")?;
                let bytes = p[r.pos..].to_vec();
                r.pos = p.len();
                Msg::Input { seq, bytes }
            }
            ty::ACK => Msg::Ack { seq: r.u64("seq")? },
            ty::RESIZE => Msg::Resize(r.size()?),
            ty::PING => Msg::Ping(r.u64("nonce")?),
            ty::PONG => Msg::Pong(r.u64("nonce")?),
            ty::DETACH => Msg::Detach,
            ty::KILL => Msg::Kill,
            ty::EXIT => Msg::Exit {
                status: r.i32("status")?,
            },
            ty::TAKEOVER => Msg::Takeover,
            ty::STATUS => Msg::Status,
            ty::STATUS_REPLY => Msg::StatusReply(StatusInfo {
                name: r.str("name")?,
                attached: r.bool("attached")?,
                identity: r.str("identity")?,
                creator: r.str("creator")?,
                created_at: r.u64("created_at")?,
                idle_secs: r.u64("idle")?,
                command: r.str("command")?,
                size: r.size()?,
                version: r.str("version")?,
                pid: r.u32("pid")?,
            }),
            ty::ERROR => Msg::Error {
                code: r.u16("code")?,
                message: r.str("message")?,
            },
            other => return Err(ProtoError::UnknownType(other)),
        };
        r.done(t)?;
        Ok(m)
    }
}

/// Incremental frame decoder: feed it bytes as they arrive, in any chunking.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    start: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete message, `Ok(None)` if more bytes are needed.
    pub fn next_msg(&mut self) -> Result<Option<Msg>, ProtoError> {
        let avail = &self.buf[self.start..];
        if avail.len() < HEADER {
            return Ok(None);
        }
        let t = avail[0];
        let len = u32::from_be_bytes(avail[1..HEADER].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(ProtoError::TooLarge(len));
        }
        if avail.len() < HEADER + len {
            return Ok(None);
        }
        let msg = Msg::decode(t, &avail[HEADER..HEADER + len])?;
        self.start += HEADER + len;
        Ok(Some(msg))
    }

    /// Bytes buffered but not yet decoded.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }
}

/// The `ACS-READY <proto>` line.
pub fn ready_line() -> String {
    format!("{READY_MARKER} {PROTO_VERSION}\n")
}

/// What the stream said before its first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// Frames follow; `rest` are the bytes already read after the marker.
    Ready { proto: u16, rest: Vec<u8> },
    /// No binary on the remote for this `uname -s` / `uname -m`.
    Need { os: String, arch: String },
}

/// Finds the marker line in a stream that may start with arbitrary noise from
/// shell startup files (DESIGN §3). The marker must start a line.
#[derive(Default)]
pub struct MarkerScanner {
    buf: Vec<u8>,
    line_start: usize,
    /// Everything before the marker, for `-v` diagnostics.
    pub noise: Vec<u8>,
}

impl MarkerScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Option<Marker> {
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = self.buf[self.line_start..].iter().position(|&b| b == b'\n') {
            let end = self.line_start + nl;
            let line = &self.buf[self.line_start..end];
            if let Some(m) = parse_marker(line) {
                let m = match m {
                    Marker::Ready { proto, .. } => Marker::Ready {
                        proto,
                        rest: self.buf[end + 1..].to_vec(),
                    },
                    need => need,
                };
                self.noise.extend_from_slice(&self.buf[..self.line_start]);
                self.buf.clear();
                self.line_start = 0;
                return Some(m);
            }
            self.line_start = end + 1;
        }
        // Keep memory bounded on a chatty login: retain the unfinished line only.
        if self.line_start > 0 {
            self.noise.extend_from_slice(&self.buf[..self.line_start]);
            self.buf.drain(..self.line_start);
            self.line_start = 0;
        }
        if self.noise.len() > 64 * 1024 {
            let cut = self.noise.len() - 64 * 1024;
            self.noise.drain(..cut);
        }
        None
    }
}

fn parse_marker(line: &[u8]) -> Option<Marker> {
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut it = line.split(' ');
    match it.next()? {
        READY_MARKER => {
            let proto = it.next()?.parse().ok()?;
            it.next().is_none().then_some(Marker::Ready {
                proto,
                rest: Vec::new(),
            })
        }
        NEED_MARKER => {
            let os = it.next()?.to_string();
            let arch = it.next()?.to_string();
            it.next().is_none().then_some(Marker::Need { os, arch })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> WinSize {
        WinSize {
            cols: 120,
            rows: 40,
            xpixel: 1200,
            ypixel: 800,
        }
    }

    fn samples() -> Vec<Msg> {
        vec![
            Msg::Hello(Hello {
                proto: PROTO_VERSION,
                session: "main".into(),
                mode: Mode::AttachOrCreate,
                identity: "michel@mbp".into(),
                force: true,
                term: "xterm-256color".into(),
                colorterm: "truecolor".into(),
                size: size(),
                resume: Some(Resume {
                    instance: u64::MAX,
                    offset: 48210,
                }),
                command: vec!["htop".into(), "-d".into(), "10".into()],
            }),
            Msg::Hello(Hello {
                proto: 7,
                session: "2".into(),
                mode: Mode::Attach,
                identity: String::new(),
                force: false,
                term: String::new(),
                colorterm: String::new(),
                size: WinSize::default(),
                resume: None,
                command: vec![],
            }),
            Msg::Welcome(Welcome {
                proto: 1,
                session: "3".into(),
                instance: 42,
                offset: 9,
                created: true,
                kind: AttachKind::Gap,
                input_seq: 12,
            }),
            Msg::Busy {
                identity: "alice@laptop".into(),
                since: 1_700_000_000,
            },
            Msg::Data {
                offset: 1 << 40,
                bytes: b"\x1b[?1049h\x00\xff hello".to_vec(),
            },
            Msg::Data {
                offset: 0,
                bytes: vec![],
            },
            Msg::Input {
                seq: 3,
                bytes: vec![0x1d, 0x1d, b'd'],
            },
            Msg::Ack { seq: 77 },
            Msg::Resize(size()),
            Msg::Ping(5),
            Msg::Pong(5),
            Msg::Detach,
            Msg::Kill,
            Msg::Exit { status: -1 },
            Msg::Takeover,
            Msg::Status,
            Msg::StatusReply(StatusInfo {
                name: "main".into(),
                attached: true,
                identity: "a@b".into(),
                creator: "c@d".into(),
                created_at: 1,
                idle_secs: 2,
                command: "zsh -l".into(),
                size: size(),
                version: "0.1.0".into(),
                pid: 4242,
            }),
            Msg::Error {
                code: err::PROTO_MISMATCH,
                message: "old master — finish or kill the session".into(),
            },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for m in samples() {
            let mut d = Decoder::new();
            d.push(&m.to_bytes());
            assert_eq!(d.next_msg().unwrap(), Some(m));
            assert_eq!(d.next_msg().unwrap(), None);
            assert_eq!(d.pending(), 0);
        }
    }

    #[test]
    fn decodes_across_every_split_point() {
        let mut stream = Vec::new();
        for m in samples() {
            m.encode(&mut stream);
        }
        for split in 0..=stream.len() {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for part in [&stream[..split], &stream[split..]] {
                d.push(part);
                while let Some(m) = d.next_msg().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, samples(), "split at {split}");
        }
    }

    #[test]
    fn decodes_byte_at_a_time() {
        let mut stream = Vec::new();
        for m in samples() {
            m.encode(&mut stream);
        }
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for b in &stream {
            d.push(std::slice::from_ref(b));
            while let Some(m) = d.next_msg().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, samples());
    }

    #[test]
    fn oversize_frame_is_rejected_from_the_header_alone() {
        let mut d = Decoder::new();
        let mut hdr = vec![ty::DATA];
        hdr.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        d.push(&hdr);
        assert_eq!(d.next_msg(), Err(ProtoError::TooLarge(MAX_FRAME + 1)));
    }

    #[test]
    fn unknown_type_and_trailing_bytes_are_errors() {
        let mut d = Decoder::new();
        d.push(&[200, 0, 0, 0, 0]);
        assert_eq!(d.next_msg(), Err(ProtoError::UnknownType(200)));

        let mut d = Decoder::new();
        d.push(&[ty::KILL, 0, 0, 0, 1, 9]);
        assert_eq!(d.next_msg(), Err(ProtoError::Trailing(ty::KILL)));

        let mut d = Decoder::new();
        d.push(&[ty::ACK, 0, 0, 0, 2, 0, 0]);
        assert_eq!(d.next_msg(), Err(ProtoError::Truncated("seq")));
    }

    #[test]
    fn bad_enum_and_utf8_values_are_errors() {
        let mut frame = Msg::Welcome(Welcome {
            proto: 1,
            session: String::new(),
            instance: 1,
            offset: 1,
            created: false,
            kind: AttachKind::Fresh,
            input_seq: 0,
        })
        .to_bytes();
        let last = frame.len() - 9;
        frame[last] = 9;
        let mut d = Decoder::new();
        d.push(&frame);
        assert_eq!(d.next_msg(), Err(ProtoError::BadValue("kind")));

        let mut frame = Msg::Busy {
            identity: "x".into(),
            since: 0,
        }
        .to_bytes();
        frame[HEADER + 4] = 0xff;
        let mut d = Decoder::new();
        d.push(&frame);
        assert_eq!(d.next_msg(), Err(ProtoError::BadUtf8));
    }

    #[test]
    fn marker_is_found_after_shell_rc_noise() {
        let mut stream = b"Welcome to Ubuntu\nlast login: yesterday\nACS-READY 1 x\n".to_vec();
        stream.extend_from_slice(&ready_line().into_bytes());
        let frames = Msg::Ping(1).to_bytes();
        stream.extend_from_slice(&frames);
        for split in 0..=stream.len() {
            let mut s = MarkerScanner::new();
            let mut got = s.push(&stream[..split]);
            let mut after = Vec::new();
            if got.is_none() {
                got = s.push(&stream[split..]);
            } else {
                after.extend_from_slice(&stream[split..]);
            }
            match got {
                Some(Marker::Ready { proto, mut rest }) => {
                    assert_eq!(proto, PROTO_VERSION);
                    rest.extend_from_slice(&after);
                    assert_eq!(rest, frames, "split at {split}");
                    assert!(s.noise.starts_with(b"Welcome to Ubuntu\n"));
                }
                other => panic!("split {split}: {other:?}"),
            }
        }
    }

    #[test]
    fn marker_must_start_a_line() {
        let mut s = MarkerScanner::new();
        assert_eq!(s.push(b"echo ACS-READY 1\n"), None);
        assert_eq!(
            s.push(b"ACS-NEED Linux aarch64\r\n"),
            Some(Marker::Need {
                os: "Linux".into(),
                arch: "aarch64".into()
            })
        );
    }

    #[test]
    fn marker_at_stream_start() {
        let mut s = MarkerScanner::new();
        assert_eq!(
            s.push(b"ACS-READY 3\n"),
            Some(Marker::Ready {
                proto: 3,
                rest: vec![]
            })
        );
        assert!(s.noise.is_empty());
    }
}
