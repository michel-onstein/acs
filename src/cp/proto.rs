//! The copy's messages (ACSCP_DESIGN §5.2) over the session's frames.
//!
//! The frames are the session's — `type u8, len u32 BE, payload`, split by
//! [`proto::Decoder::next_frame`] and written with the same field codec —
//! but the message set is the copy's own enum, so a master can never be
//! handed a copy frame nor a receiver a session frame: two types, not a
//! runtime check (decision 11). `PING` and `PONG` are the exception: they
//! are the session's frames, byte for byte, because `reconnect::Liveness`
//! writes its pings as the session's `Msg::Ping` (decision 12).

use crate::proto::{self, ProtoError, HEADER, MAX_CHUNK, R, W};

/// The longest name a `DIR` or `FILE` may carry, in bytes: `NAME_MAX` on
/// Linux and macOS alike.
pub const MAX_NAME: usize = 255;

/// The longest message a `FAIL`, `SKIP` or `ERROR` carries, in bytes. A
/// longer one is cut on the way out and refused on the way in.
pub const MAX_TEXT: usize = 1024;

/// The most source paths a `HELLO` may name.
pub const MAX_PATHS: usize = 4096;

/// The frame types. `PING` and `PONG` are the session's numbers.
pub mod ty {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const ERROR: u8 = 3;
    pub const DATA: u8 = 4;
    pub const DIR: u8 = 5;
    pub const DIR_END: u8 = 6;
    pub const FILE: u8 = 7;
    pub const PING: u8 = 8;
    pub const PONG: u8 = 9;
    pub const FILE_END: u8 = 10;
    pub const SKIP: u8 = 11;
    pub const END: u8 = 12;
    pub const GO: u8 = 13;
    pub const OK: u8 = 14;
    pub const FAIL: u8 = 15;
    pub const DONE: u8 = 16;
    pub const HAVE: u8 = 17;
    pub const RESTART: u8 = 18;
}

/// What the remote end of a copy is asked to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Walk the paths and send them.
    Send,
    /// Write what arrives under the target.
    Receive,
}

/// The first message, client to remote end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The crate version: both ends are the same build, and a remote end
    /// reached through an unexpected path refuses a mismatch by name.
    pub version: String,
    pub role: Role,
    pub recursive: bool,
    pub preserve: bool,
    pub verbose: bool,
    /// For a receiver, where to write; empty for a sender.
    pub target: String,
    /// For a sender, the source paths as typed; for a receiver, the
    /// top-level names it is to accept (ACSCP_DESIGN §5.4).
    pub paths: Vec<String>,
}

/// A file's or a directory's time, as `stat` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub secs: i64,
    pub nanos: u32,
}

/// What `-p` carries of an entry: its mode and its two times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meta {
    /// The permission bits, `0o7777` at most.
    pub mode: u32,
    pub mtime: Time,
    pub atime: Time,
}

/// One copy message. `Data` borrows its bytes from the decoder's buffer,
/// so a chunk is written to the part file straight from where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg<'a> {
    Hello(Hello),
    Welcome {
        umask: u32,
        target_is_dir: bool,
    },
    Error {
        code: u16,
        message: String,
    },
    Dir {
        name: String,
        meta: Meta,
    },
    DirEnd,
    File {
        id: u32,
        name: String,
        size: u64,
        meta: Meta,
    },
    Data {
        id: u32,
        bytes: &'a [u8],
    },
    FileEnd {
        id: u32,
    },
    Skip {
        name: String,
        reason: String,
    },
    End,
    Go {
        id: u32,
        offset: u64,
    },
    Ok {
        id: u32,
    },
    /// `id` is 0 for an entry that is not a file (a refused directory),
    /// which `name` then names.
    Fail {
        id: u32,
        name: String,
        message: String,
    },
    Done {
        written: u64,
        failed: u64,
    },
    /// The receiver holds the first `len` bytes of file `id` already, in
    /// its part, whose SHA-256 is `hash` (ACSCP_DESIGN §7): the answer to
    /// a `FILE` instead of `GO`.
    Have {
        id: u32,
        len: u64,
        hash: [u8; 32],
    },
    /// The part is not the start of file `id`: truncate it, its `DATA`
    /// follows from 0.
    Restart {
        id: u32,
    },
    Ping(u64),
    Pong(u64),
}

/// The error codes an `ERROR` carries.
pub mod err {
    /// The two ends are not the same build.
    pub const VERSION: u16 = 1;
    /// The request cannot be met: several sources into a target that is
    /// not a directory, a target that cannot be looked at.
    pub const REQUEST: u16 = 2;
    /// The far end broke the protocol: a bad name, a frame out of place.
    pub const PROTOCOL: u16 = 3;
}

/// Whether `name` may be one entry's name: one path component, non-empty,
/// not `.` or `..`, no `/`, no NUL, at most [`MAX_NAME`] bytes
/// (ACSCP_DESIGN §5.4). Anything else from the far end ends the copy, so
/// the receiver's current directory is always inside its target.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && !name.bytes().any(|b| b == b'/' || b == 0)
}

/// `s` cut to [`MAX_TEXT`] bytes on a character boundary.
fn bounded(s: &str) -> &str {
    if s.len() <= MAX_TEXT {
        return s;
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn put_time(w: &mut W<'_>, t: Time) {
    w.u64(t.secs as u64);
    w.u32(t.nanos);
}

fn put_meta(w: &mut W<'_>, m: &Meta) {
    w.u32(m.mode);
    put_time(w, m.mtime);
    put_time(w, m.atime);
}

fn get_time(r: &mut R<'_>, what: &'static str) -> Result<Time, ProtoError> {
    let secs = r.u64(what)? as i64;
    let nanos = r.u32(what)?;
    if nanos >= 1_000_000_000 {
        return Err(ProtoError::BadValue(what));
    }
    Ok(Time { secs, nanos })
}

fn get_meta(r: &mut R<'_>) -> Result<Meta, ProtoError> {
    let mode = r.u32("mode")?;
    if mode > 0o7777 {
        return Err(ProtoError::BadValue("mode"));
    }
    Ok(Meta {
        mode,
        mtime: get_time(r, "mtime")?,
        atime: get_time(r, "atime")?,
    })
}

fn get_name(r: &mut R<'_>) -> Result<String, ProtoError> {
    let s = r.str("name")?;
    match valid_name(&s) {
        true => Ok(s),
        false => Err(ProtoError::BadValue("name")),
    }
}

fn get_text(r: &mut R<'_>, what: &'static str) -> Result<String, ProtoError> {
    let s = r.str(what)?;
    match s.len() <= MAX_TEXT {
        true => Ok(s),
        false => Err(ProtoError::BadValue(what)),
    }
}

/// Start a frame of type `t` in `out`, returning where its length goes.
fn begin(t: u8, out: &mut Vec<u8>) -> usize {
    out.push(t);
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    at
}

/// Write the length of the frame begun at `at`.
fn finish(at: usize, out: &mut [u8]) {
    let len = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

impl Msg<'_> {
    /// Append this message as one frame.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let t = self.ty();
        let at = begin(t, out);
        let mut w = W(out);
        match self {
            Msg::Hello(h) => {
                w.str(&h.version);
                w.u8(match h.role {
                    Role::Send => 0,
                    Role::Receive => 1,
                });
                w.u8(h.recursive as u8 | (h.preserve as u8) << 1 | (h.verbose as u8) << 2);
                w.str(&h.target);
                w.u32(h.paths.len() as u32);
                for p in &h.paths {
                    w.str(p);
                }
            }
            Msg::Welcome {
                umask,
                target_is_dir,
            } => {
                w.u32(*umask);
                w.u8(*target_is_dir as u8);
            }
            Msg::Error { code, message } => {
                w.u16(*code);
                w.str(bounded(message));
            }
            Msg::Dir { name, meta } => {
                w.str(name);
                put_meta(&mut w, meta);
            }
            Msg::File {
                id,
                name,
                size,
                meta,
            } => {
                w.u32(*id);
                w.str(name);
                w.u64(*size);
                put_meta(&mut w, meta);
            }
            Msg::Data { id, bytes } => {
                w.u32(*id);
                w.0.extend_from_slice(bytes);
            }
            Msg::FileEnd { id } | Msg::Ok { id } | Msg::Restart { id } => w.u32(*id),
            Msg::Have { id, len, hash } => {
                w.u32(*id);
                w.u64(*len);
                w.0.extend_from_slice(hash);
            }
            Msg::Skip { name, reason } => {
                w.str(bounded(name));
                w.str(bounded(reason));
            }
            Msg::Go { id, offset } => {
                w.u32(*id);
                w.u64(*offset);
            }
            Msg::Fail { id, name, message } => {
                w.u32(*id);
                w.str(bounded(name));
                w.str(bounded(message));
            }
            Msg::Done { written, failed } => {
                w.u64(*written);
                w.u64(*failed);
            }
            Msg::Ping(n) | Msg::Pong(n) => w.u64(*n),
            Msg::DirEnd | Msg::End => {}
        }
        finish(at, out);
    }

    /// This message's frame type.
    pub fn ty(&self) -> u8 {
        match self {
            Msg::Hello(_) => ty::HELLO,
            Msg::Welcome { .. } => ty::WELCOME,
            Msg::Error { .. } => ty::ERROR,
            Msg::Dir { .. } => ty::DIR,
            Msg::DirEnd => ty::DIR_END,
            Msg::File { .. } => ty::FILE,
            Msg::Data { .. } => ty::DATA,
            Msg::FileEnd { .. } => ty::FILE_END,
            Msg::Skip { .. } => ty::SKIP,
            Msg::End => ty::END,
            Msg::Go { .. } => ty::GO,
            Msg::Ok { .. } => ty::OK,
            Msg::Fail { .. } => ty::FAIL,
            Msg::Done { .. } => ty::DONE,
            Msg::Have { .. } => ty::HAVE,
            Msg::Restart { .. } => ty::RESTART,
            Msg::Ping(_) => ty::PING,
            Msg::Pong(_) => ty::PONG,
        }
    }

    /// The message one frame of type `t` with payload `p` holds.
    pub fn decode(t: u8, p: &[u8]) -> Result<Msg<'_>, ProtoError> {
        let mut r = R::new(p);
        let m = match t {
            ty::HELLO => {
                let version = r.str("version")?;
                let role = match r.u8("role")? {
                    0 => Role::Send,
                    1 => Role::Receive,
                    _ => return Err(ProtoError::BadValue("role")),
                };
                let flags = r.u8("flags")?;
                if flags > 0b111 {
                    return Err(ProtoError::BadValue("flags"));
                }
                let target = r.str("target")?;
                let n = r.u32("paths")? as usize;
                if n > MAX_PATHS {
                    return Err(ProtoError::BadValue("paths"));
                }
                let mut paths = Vec::with_capacity(n);
                for _ in 0..n {
                    paths.push(r.str("path")?);
                }
                Msg::Hello(Hello {
                    version,
                    role,
                    recursive: flags & 1 != 0,
                    preserve: flags & 2 != 0,
                    verbose: flags & 4 != 0,
                    target,
                    paths,
                })
            }
            ty::WELCOME => Msg::Welcome {
                umask: r.u32("umask")?,
                target_is_dir: r.bool("target_is_dir")?,
            },
            ty::ERROR => Msg::Error {
                code: r.u16("code")?,
                message: get_text(&mut r, "message")?,
            },
            ty::DIR => Msg::Dir {
                name: get_name(&mut r)?,
                meta: get_meta(&mut r)?,
            },
            ty::DIR_END => Msg::DirEnd,
            ty::FILE => Msg::File {
                id: r.u32("id")?,
                name: get_name(&mut r)?,
                size: r.u64("size")?,
                meta: get_meta(&mut r)?,
            },
            ty::DATA => {
                let id = r.u32("id")?;
                let bytes = &p[4..];
                if bytes.len() > MAX_CHUNK {
                    return Err(ProtoError::BadValue("data"));
                }
                return Ok(Msg::Data { id, bytes });
            }
            ty::FILE_END => Msg::FileEnd { id: r.u32("id")? },
            ty::SKIP => Msg::Skip {
                name: get_text(&mut r, "name")?,
                reason: get_text(&mut r, "reason")?,
            },
            ty::END => Msg::End,
            ty::GO => Msg::Go {
                id: r.u32("id")?,
                offset: r.u64("offset")?,
            },
            ty::OK => Msg::Ok { id: r.u32("id")? },
            ty::FAIL => Msg::Fail {
                id: r.u32("id")?,
                name: get_text(&mut r, "name")?,
                message: get_text(&mut r, "message")?,
            },
            ty::DONE => Msg::Done {
                written: r.u64("written")?,
                failed: r.u64("failed")?,
            },
            ty::HAVE => Msg::Have {
                id: r.u32("id")?,
                len: r.u64("len")?,
                hash: r.take(32, "hash")?.try_into().expect("32 bytes taken"),
            },
            ty::RESTART => Msg::Restart { id: r.u32("id")? },
            ty::PING => Msg::Ping(r.u64("nonce")?),
            ty::PONG => Msg::Pong(r.u64("nonce")?),
            _ => return Err(ProtoError::UnknownType(t)),
        };
        r.done(t)?;
        Ok(m)
    }

    /// Next message from `dec`, `Ok(None)` if more bytes are needed.
    pub fn next(dec: &mut proto::Decoder) -> Result<Option<Msg<'_>>, ProtoError> {
        match dec.next_frame()? {
            Some((t, p)) => Msg::decode(t, p).map(Some),
            None => Ok(None),
        }
    }
}

/// Append a `DATA` frame for file `id` whose payload is read straight into
/// `out` by `read`, at most [`MAX_CHUNK`] bytes of it: the chunk goes from
/// the file into the link's queue with no copy in between (ACSCP_DESIGN
/// §5.1). Returns how many bytes `read` gave; on 0 or an error nothing is
/// left appended.
pub fn encode_data_from(
    id: u32,
    max: usize,
    out: &mut Vec<u8>,
    read: impl FnOnce(&mut [u8]) -> std::io::Result<usize>,
) -> std::io::Result<usize> {
    let start = out.len();
    let want = max.min(MAX_CHUNK);
    out.reserve(HEADER + 4 + want);
    let at = begin(ty::DATA, out);
    out.extend_from_slice(&id.to_be_bytes());
    let body = out.len();
    out.resize(body + want, 0);
    let n = match read(&mut out[body..]) {
        Ok(n) if n > 0 => n.min(want),
        other => {
            out.truncate(start);
            return other;
        }
    };
    out.truncate(body + n);
    finish(at, out);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta {
            mode: 0o644,
            mtime: Time {
                secs: 1_700_000_000,
                nanos: 123_456_789,
            },
            atime: Time { secs: -5, nanos: 0 },
        }
    }

    fn samples() -> Vec<Msg<'static>> {
        vec![
            Msg::Hello(Hello {
                version: "0.17.0".into(),
                role: Role::Receive,
                recursive: true,
                preserve: false,
                verbose: true,
                target: "/tmp/x y".into(),
                paths: vec!["a".into(), "ünï".into()],
            }),
            Msg::Hello(Hello {
                version: "1".into(),
                role: Role::Send,
                recursive: false,
                preserve: true,
                verbose: false,
                target: String::new(),
                paths: vec![],
            }),
            Msg::Welcome {
                umask: 0o022,
                target_is_dir: true,
            },
            Msg::Error {
                code: err::REQUEST,
                message: "not a directory".into(),
            },
            Msg::Dir {
                name: "d".into(),
                meta: meta(),
            },
            Msg::DirEnd,
            Msg::File {
                id: 7,
                name: "f.txt".into(),
                size: u64::MAX,
                meta: meta(),
            },
            Msg::Data {
                id: 7,
                bytes: b"\0\x1b[2Jbytes",
            },
            Msg::Data { id: 1, bytes: b"" },
            Msg::FileEnd { id: 7 },
            Msg::Skip {
                name: "fifo".into(),
                reason: "not a regular file".into(),
            },
            Msg::End,
            Msg::Go { id: 7, offset: 9 },
            Msg::Ok { id: 7 },
            Msg::Fail {
                id: 0,
                name: "x".into(),
                message: "no".into(),
            },
            Msg::Done {
                written: 3,
                failed: 1,
            },
            Msg::Have {
                id: 7,
                len: 1 << 40,
                hash: [0xa5; 32],
            },
            Msg::Restart { id: 7 },
            Msg::Ping(5),
            Msg::Pong(u64::MAX),
        ]
    }

    #[test]
    fn every_message_round_trips_across_every_split_point() {
        let mut bytes = Vec::new();
        for m in samples() {
            m.encode(&mut bytes);
        }
        for split in 0..=bytes.len() {
            let mut dec = proto::Decoder::new();
            let mut got = Vec::new();
            for part in [&bytes[..split], &bytes[split..]] {
                dec.push(part);
                while let Some(m) = Msg::next(&mut dec).unwrap() {
                    let mut again = Vec::new();
                    m.encode(&mut again);
                    got.push(again);
                }
            }
            let want: Vec<Vec<u8>> = samples()
                .iter()
                .map(|m| {
                    let mut v = Vec::new();
                    m.encode(&mut v);
                    v
                })
                .collect();
            assert_eq!(got, want, "split at {split}");
        }
    }

    /// The decoded value is the encoded one, field for field.
    #[test]
    fn decode_gives_back_what_was_encoded() {
        for m in samples() {
            let mut b = Vec::new();
            m.encode(&mut b);
            assert_eq!(Msg::decode(b[0], &b[HEADER..]).unwrap(), m);
        }
    }

    /// A frame cut short anywhere is refused, never misread.
    #[test]
    fn truncated_payloads_are_errors() {
        for m in samples() {
            let mut b = Vec::new();
            m.encode(&mut b);
            let p = &b[HEADER..];
            for cut in 0..p.len() {
                // DATA's bytes may be any length; only its id can be cut.
                if m.ty() == ty::DATA && cut >= 4 {
                    continue;
                }
                assert!(Msg::decode(b[0], &p[..cut]).is_err(), "{m:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn trailing_bytes_and_unknown_types_are_errors() {
        let mut b = Vec::new();
        Msg::End.encode(&mut b);
        assert_eq!(
            Msg::decode(ty::END, &[0]),
            Err(ProtoError::Trailing(ty::END))
        );
        assert_eq!(Msg::decode(99, &[]), Err(ProtoError::UnknownType(99)));
        assert_eq!(
            Msg::decode(ty::GO, &[0; 13]),
            Err(ProtoError::Trailing(ty::GO))
        );
    }

    /// The copy's PING and PONG are the session's frames, byte for byte,
    /// since `reconnect::Liveness` writes its pings as `Msg::Ping`.
    #[test]
    fn ping_and_pong_are_the_sessions_frames() {
        for n in [0, 1, 0xdead_beef_u64, u64::MAX] {
            let mut ours = Vec::new();
            Msg::Ping(n).encode(&mut ours);
            assert_eq!(ours, proto::Msg::Ping(n).to_bytes());
            let mut ours = Vec::new();
            Msg::Pong(n).encode(&mut ours);
            assert_eq!(ours, proto::Msg::Pong(n).to_bytes());
        }
    }

    /// ACSCP_DESIGN §5.4, as a table.
    #[test]
    fn names_are_single_components() {
        let long = "x".repeat(MAX_NAME);
        let too_long = "x".repeat(300);
        for (name, ok) in [
            ("a", true),
            ("a.txt", true),
            (".bashrc", true),
            ("...", true),
            ("ünïcødé ☃", true),
            (long.as_str(), true),
            ("", false),
            (".", false),
            ("..", false),
            ("a/b", false),
            ("/", false),
            ("a/", false),
            ("../x", false),
            ("a\0b", false),
            (too_long.as_str(), false),
        ] {
            assert_eq!(valid_name(name), ok, "{name:?}");
        }
    }

    /// A bad name in a DIR or FILE fails the frame itself.
    #[test]
    fn a_bad_name_on_the_wire_is_refused() {
        for bad in ["..", "a/b", "", "x\0"] {
            for m in [
                Msg::Dir {
                    name: bad.into(),
                    meta: meta(),
                },
                Msg::File {
                    id: 1,
                    name: bad.into(),
                    size: 0,
                    meta: meta(),
                },
            ] {
                let mut b = Vec::new();
                m.encode(&mut b);
                assert_eq!(
                    Msg::decode(b[0], &b[HEADER..]),
                    Err(ProtoError::BadValue("name")),
                    "{bad:?}"
                );
            }
        }
    }

    #[test]
    fn modes_times_and_texts_are_bounded() {
        let mut m = meta();
        m.mode = 0o10000;
        let mut b = Vec::new();
        Msg::Dir {
            name: "d".into(),
            meta: m,
        }
        .encode(&mut b);
        assert_eq!(
            Msg::decode(b[0], &b[HEADER..]),
            Err(ProtoError::BadValue("mode"))
        );

        // A long message is cut on the way out, at a character boundary.
        let mut b = Vec::new();
        let long = "é".repeat(MAX_TEXT);
        Msg::Error {
            code: 1,
            message: long,
        }
        .encode(&mut b);
        let Msg::Error { message, .. } = Msg::decode(b[0], &b[HEADER..]).unwrap() else {
            panic!()
        };
        assert!(message.len() <= MAX_TEXT && message.len() >= MAX_TEXT - 1);
    }

    #[test]
    fn data_read_straight_into_the_queue_is_a_data_frame() {
        let mut out = vec![1, 2, 3];
        let n = encode_data_from(4, 10, &mut out, |buf| {
            buf[..3].copy_from_slice(b"abc");
            Ok(3)
        })
        .unwrap();
        assert_eq!(n, 3);
        let mut want = vec![1, 2, 3];
        Msg::Data {
            id: 4,
            bytes: b"abc",
        }
        .encode(&mut want);
        assert_eq!(out, want);

        // Nothing read, or an error: nothing appended.
        assert_eq!(encode_data_from(4, 10, &mut out, |_| Ok(0)).unwrap(), 0);
        assert!(
            encode_data_from(4, 10, &mut out, |_| Err(std::io::ErrorKind::Other.into())).is_err()
        );
        assert_eq!(out, want);

        // Never more than a chunk, whatever is asked.
        let mut out = Vec::new();
        let n = encode_data_from(1, usize::MAX, &mut out, |buf| Ok(buf.len())).unwrap();
        assert_eq!(n, MAX_CHUNK);
        assert_eq!(out.len(), HEADER + 4 + MAX_CHUNK);
    }
}
