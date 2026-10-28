//! A copy between two hosts (ACSCP_DESIGN §5.6, decision 10), routed
//! through this machine as `scp -3` routes one: an `acs _cp` sender on the
//! source host, an `acs _cp` receiver on the target host, and this process
//! splicing the two links' frames, as `_proxy` splices a session's.
//!
//! Nothing here opens a file. Each frame is decoded on its way through,
//! which is the receiver's own check of a name — one path component, never
//! `..` or a `/` — so a name that could leave the target ends the copy here
//! before the target host sees it; the receiver there accepts at the top
//! level only the names asked for (§5.4). Each host may send only its own
//! direction's messages: a receiver that sends a `FILE` is broken, not
//! obeyed. Beyond that only the `FILE` headers are read, for the meter, and
//! the answers, for the report. `PING` and `PONG` are each link's own.
//!
//! A lost link ends the copy: the redial of §7 wants an endpoint to resume
//! from, and the relay is neither. The part stays on the target host, so
//! the same command run again goes on from it, by the `HAVE` the receiver
//! answers its `FILE` with — which passes through here like any other
//! frame.

use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use super::cli::{CopyArgs, Remote, Spec};
use super::progress::Progress;
use super::proto::{err, ty, Msg, Role};
use super::{dial_cp, hello, note, note_on, prepare, wanted_name, Dialed, Options, Outcome, HIGH};
use crate::client::{code, Link};
use crate::outq::OutQueue;
use crate::proto::Decoder;
use crate::reconnect::{Health, Liveness};
use crate::sys;

/// What a sender may send once it has said `WELCOME`.
const FROM_SENDER: &[u8] = &[
    ty::DIR,
    ty::DIR_END,
    ty::FILE,
    ty::DATA,
    ty::FILE_END,
    ty::SKIP,
    ty::END,
    ty::RESTART,
];

/// What a receiver may send once it has said `WELCOME`.
const FROM_RECEIVER: &[u8] = &[ty::GO, ty::HAVE, ty::OK, ty::FAIL, ty::DONE];

/// One of the two links.
struct Side {
    link: Link,
    dest: String,
    dec: Decoder,
    out: OutQueue,
    live: Liveness,
    /// The far end closed its output when that was no loss: the receiver
    /// after its `DONE`, the sender after its `END`.
    eof: bool,
    /// Its `WELCOME`, once it came: whether the target is a directory.
    welcome: Option<bool>,
    may: &'static [u8],
}

impl Side {
    fn r(&self) -> RawFd {
        self.link.from_fd().as_raw_fd()
    }

    fn w(&self) -> RawFd {
        self.link.to_fd().as_raw_fd()
    }

    /// The far end broke the protocol: tell it, as `drive` does, and say
    /// which host it was.
    fn broke(&mut self, m: String) -> String {
        Msg::Error {
            code: err::PROTOCOL,
            message: m.clone(),
        }
        .encode(self.out.tail());
        let _ = self.out.flush(self.w());
        format!("protocol error from {}: {m}", self.dest)
    }

    /// The link went before the copy was done, which ends it.
    fn lost(&self, why: &str) -> String {
        format!(
            "{}: {why}; a copy between two hosts does not reconnect — run it again to go on from where it got to",
            self.dest
        )
    }
}

/// Dial `acs _cp` on `host` and send it `hello`; the exit code, told
/// already, when it does not come up.
fn open(a: &CopyArgs, host: &Remote, hello: &[u8], may: &'static [u8]) -> Result<Side, u8> {
    let mut conn = prepare(a, host)?;
    match dial_cp(&mut conn, false, hello, None) {
        Dialed::Up(mut link, rest) => {
            let mut dec = Decoder::new();
            dec.push(&rest);
            let mut out = OutQueue::default();
            *out.tail() = link.take_pending();
            Ok(Side {
                link,
                dest: host.destination(),
                dec,
                out,
                live: Liveness::new(),
                eof: false,
                welcome: None,
                may,
            })
        }
        Dialed::Down(e) => {
            crate::client::note(&e);
            Err(code::UNREACHABLE)
        }
        Dialed::Refused(e, c) => {
            crate::client::note(&e);
            Err(c)
        }
    }
}

/// The relay's own state: what it has seen go by.
struct Relay {
    outcome: Outcome,
    /// The sender's `END` went by.
    ended: bool,
    /// The receiver's `DONE` came and was passed on.
    done: bool,
    /// More than one source in all, or a wildcard that may match several:
    /// the target must be a directory.
    need_dir: bool,
    meter: Option<Progress>,
    /// The headers seen and not yet ended, for the meter: id, name and
    /// size. A window's worth at most, so a list.
    files: Vec<(u32, String, u64)>,
    /// The file the meter's line is for.
    on: Option<u32>,
}

impl Relay {
    /// End the meter's line, if one is drawn.
    fn end_line(&mut self) {
        if let (Some(_), Some(p)) = (self.on.take(), &mut self.meter) {
            p.finish();
        }
    }

    /// Draw the meter's line for file `id`, ending another's.
    fn show(&mut self, id: u32) {
        if self.on == Some(id) {
            return;
        }
        self.end_line();
        let file = self.files.iter().find(|f| f.0 == id);
        if let (Some(p), Some((_, name, size))) = (&mut self.meter, file) {
            p.start(name, *size);
            self.on = Some(id);
        }
    }

    /// Note what `m`, on its way through, says about the copy.
    fn peek(&mut self, m: &Msg<'_>) {
        match *m {
            Msg::File {
                id, ref name, size, ..
            } => {
                if self.meter.is_some() {
                    self.files.push((id, name.clone(), size));
                }
            }
            Msg::Data { id, bytes } => {
                self.outcome.bytes += bytes.len() as u64;
                self.show(id);
                if let Some(p) = &mut self.meter {
                    p.add(bytes.len() as u64);
                }
            }
            Msg::FileEnd { id } => {
                self.show(id);
                self.end_line();
                self.files.retain(|f| f.0 != id);
            }
            Msg::Skip {
                ref name,
                ref reason,
            } => {
                note_on(name, reason);
                self.outcome.skipped += 1;
            }
            Msg::End => self.ended = true,
            Msg::Ok { .. } => self.outcome.written += 1,
            Msg::Fail {
                id,
                ref name,
                ref message,
            } => {
                if self.on == Some(id) {
                    self.end_line();
                }
                self.files.retain(|f| f.0 != id);
                note_on(name, message);
                self.outcome.failed += 1;
            }
            Msg::Done { written, failed } => {
                self.outcome.written = written;
                self.outcome.failed = failed;
                self.done = true;
            }
            _ => {}
        }
    }

    /// Pass on every whole frame `from` has read to `to`, answering and
    /// hearing `from`'s pings itself.
    fn pass(&mut self, from: &mut Side, to: &mut Side) -> Result<(), String> {
        loop {
            let m = match Msg::next(&mut from.dec) {
                Ok(Some(m)) => m,
                Ok(None) => return Ok(()),
                Err(e) => {
                    let e = e.to_string();
                    return Err(from.broke(e));
                }
            };
            let t = m.ty();
            match m {
                Msg::Ping(n) => Msg::Pong(n).encode(from.out.tail()),
                Msg::Pong(n) => from.live.pong(n),
                Msg::Error { message, .. } => return Err([&from.dest, ": ", &message].concat()),
                Msg::Welcome { target_is_dir, .. } if from.welcome.is_none() => {
                    from.welcome = Some(target_is_dir);
                }
                _ if from.welcome.is_none() || !from.may.contains(&t) => {
                    return Err(from.broke(format!("unexpected message type {t}")));
                }
                m => {
                    self.peek(&m);
                    m.encode(to.out.tail());
                }
            }
        }
    }

    /// Read what `from` has and pass it on to `to`.
    fn read(&mut self, from: &mut Side, to: &mut Side, buf: &mut [u8]) -> Result<(), String> {
        match sys::read(from.r(), buf) {
            Ok(0) if self.done || (self.ended && from.may == FROM_SENDER) => from.eof = true,
            Ok(0) => return Err(from.lost("the link closed before the copy finished")),
            Ok(n) => {
                from.live.heard();
                from.dec.push(&buf[..n]);
                self.pass(from, to)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(from.lost(&format!("the link failed: {e}"))),
        }
        Ok(())
    }

    /// Splice `s`, the sender's link, and `r`, the receiver's, until the
    /// receiver's `DONE` has reached the sender.
    fn splice(&mut self, s: &mut Side, r: &mut Side, target: &str) -> Result<(), String> {
        for fd in [s.r(), s.w(), r.r(), r.w()] {
            sys::set_nonblocking(fd, true).map_err(|e| e.to_string())?;
        }
        let mut buf = vec![0u8; 64 * 1024];
        // What came after the markers.
        self.pass(s, r)?;
        self.pass(r, s)?;
        loop {
            if self.need_dir && r.welcome == Some(false) {
                return Err([&r.dest, ":", target, ": not a directory"].concat());
            }
            if self.done && (s.out.is_empty() || s.eof) {
                return Ok(());
            }
            // Read a side only while the other has room for what it sends:
            // a slow target holds the source back rather than filling
            // memory here. The side held back is not to blame for its
            // silence meanwhile.
            let (s_in, r_in) = (!s.eof && r.out.len() < HIGH, !r.eof && s.out.len() < HIGH);
            let pollin = |on: bool| if on { libc::POLLIN } else { 0 };
            let pollout = |q: &OutQueue| if q.is_empty() { 0 } else { libc::POLLOUT };
            let mut fds = [
                sys::wait_for(s.r(), pollin(s_in)),
                sys::wait_for(s.w(), pollout(&s.out)),
                sys::wait_for(r.r(), pollin(r_in)),
                sys::wait_for(r.w(), pollout(&r.out)),
            ];
            let next = s.live.next_deadline_ms().min(r.live.next_deadline_ms());
            let wait = Duration::from_millis(next.saturating_sub(sys::now_ms()));
            sys::poll(&mut fds, sys::poll_timeout(wait)).map_err(|e| e.to_string())?;
            if fds[0].revents != 0 {
                self.read(s, r, &mut buf)?;
            }
            if fds[2].revents != 0 {
                self.read(r, s, &mut buf)?;
            }
            for (side, i) in [(&mut *s, 1), (&mut *r, 3)] {
                if fds[i].revents != 0 && !side.out.flush(side.w()) {
                    return Err(side.lost("the link closed before the copy finished"));
                }
            }
            for (side, held) in [(&mut *s, !s_in), (&mut *r, !r_in)] {
                if held {
                    side.live.heard();
                }
                if !side.eof && side.live.tick(side.out.tail()) == Health::Dead {
                    return Err(side.lost("no answer from the host"));
                }
            }
        }
    }
}

/// `acscp h1:a h2:b`: every source on a host, the target on a host. Each
/// source host's sources are one relay to the target host, run in turn in
/// the order the command line names them (§5.6).
pub(super) fn copy(a: &CopyArgs, dst: &Remote, opts: &Options) -> Result<Outcome, u8> {
    // Each source host with its paths and the names they arrive under.
    let mut groups: Vec<(&Remote, Vec<String>, Vec<String>)> = Vec::new();
    for s in &a.sources {
        let Spec::Remote(src) = s else { continue };
        let Some(name) = wanted_name(&src.path) else {
            note(&format!(
                "{}:{}: name the file or directory to copy, not the one it is in",
                src.destination(),
                src.path
            ));
            return Err(code::USAGE);
        };
        let at = match groups
            .iter()
            .position(|g| g.0.destination() == src.destination())
        {
            Some(at) => at,
            None => {
                groups.push((src, Vec::new(), Vec::new()));
                groups.len() - 1
            }
        };
        groups[at].1.push(src.path.clone());
        groups[at].2.push(name);
    }
    let mut relay = Relay {
        outcome: Outcome::default(),
        ended: false,
        done: false,
        need_dir: a.sources.len() > 1
            || a.sources.iter().any(|s| match s {
                Spec::Remote(r) => super::glob::is_pattern(&r.path),
                Spec::Local(_) => false,
            }),
        meter: opts.meter.then(Progress::on_terminal).flatten(),
        files: Vec::new(),
        on: None,
    };
    let mut total = Outcome::default();
    for (src, paths, names) in groups {
        let to = hello(Role::Receive, &dst.path, names, opts);
        let mut r = open(a, dst, &to, FROM_RECEIVER)?;
        let mut s = open(a, src, &hello(Role::Send, "", paths, opts), FROM_SENDER)?;
        let res = relay.splice(&mut s, &mut r, &dst.path);
        relay.end_line();
        s.link.close();
        r.link.close();
        let o = std::mem::take(&mut relay.outcome);
        relay.done = false;
        relay.ended = false;
        relay.files.clear();
        total.written += o.written;
        total.failed += o.failed;
        total.skipped += o.skipped;
        total.bytes += o.bytes;
        if let Err(e) = res {
            total.error = Some(e);
            break;
        }
    }
    Ok(total)
}
