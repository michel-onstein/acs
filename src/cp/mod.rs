//! `acscp` (ACSCP_DESIGN.md): `acs` under a second name, copying files and
//! directories as `scp` does.
//!
//! Two endpoints talk the copy protocol ([`proto`]): a [`sender`] that
//! walks files and streams them, and a [`receiver`] that writes them. Each
//! is a state machine driven by [`drive`], a `poll` loop over the link's
//! descriptors that writes through an [`OutQueue`]. A copy with no host in
//! it runs both in one process over a socketpair, which is also how the
//! tests exercise the protocol with no transport at all.
//!
//! A copy to or from a host runs one endpoint here and the other in
//! [`remote`], `acs _cp`, at the end of the session's own dial (§3, §8).
//!
//! A copy survives its link (§7): a lost one is redialled on the
//! session's backoff, and the receiver answers a file whose part is on
//! disk with `HAVE` — its length and a hash of it — so the sender goes on
//! after it, or `RESTART`s it when it is not the file's start. The part is
//! the only state, so a later run resumes an earlier one the same way.
//!
//! A copy between two hosts runs both endpoints remotely, one on each,
//! and [`relay`] splices their links here (§5.6).
//!
//! A remote source may hold wildcards, which `acs _cp` expands itself
//! ([`glob`]): there is no remote shell (decision 4).
//!
//! Built so far: phases 1–3 of the design's plan (§12) — the protocol,
//! the two endpoints, local copies, uploads and downloads with one host,
//! and resume — and phase 5: the relay between two hosts and remote
//! wildcards. Local and remote sources in one copy, and downloads from
//! several hosts, run those pieces in turn (DESIGN decision 17).

pub mod cli;
pub mod glob;
pub mod progress;
pub mod proto;
pub mod receiver;
mod relay;
pub mod remote;
pub mod sender;
pub mod walk;

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::client::code;
use crate::netwatch::NetWatch;
use crate::outq::OutQueue;
use crate::proto::Decoder;
use crate::reconnect::{Backoff, Health, Liveness};
use crate::sha256::Sha256;
use crate::ssh::Call;
use crate::sys;
use crate::timing::{Phase, Timing};
use cli::{CopyArgs, Parsed, Spec};
use proto::{Msg, Role};

/// The program name the copy's messages carry.
pub const NAME: &str = "acscp";

/// Stop producing output while this much is queued: the next chunk is read
/// from disk only when the link has taken the last ones (ACSCP_DESIGN §5.3).
const HIGH: usize = 256 * 1024;

/// The file headers a sender runs ahead of their data by default
/// (`ACS_CP_WINDOW`, ACSCP_DESIGN §5.3).
pub const WINDOW: usize = 64;

/// The most file headers a sender runs ahead, whatever `ACS_CP_WINDOW`
/// says. Each holds its source open, and in a local copy the receiver's
/// parts are in the same process: this many plus [`OPEN_PARTS`] stays well
/// under macOS's soft limit of 256 descriptors (ACSCP_DESIGN §5.3, acs-d2kt).
pub const MAX_WINDOW: usize = 96;

/// The most parts a receiver holds open. A `FILE` past them is answered
/// once one closes, not failed, so a sender with a larger window (an older
/// one, or one told so) only waits (ACSCP_DESIGN §5.3, acs-d2kt).
pub const OPEN_PARTS: usize = 64;

// Both together, with room for the rest, under macOS's soft limit of 256.
const _: () = assert!(MAX_WINDOW + OPEN_PARTS < 200);

/// The window `ACS_CP_WINDOW` asks for, kept between 1 and [`MAX_WINDOW`].
fn window(asked: usize) -> usize {
    asked.clamp(1, MAX_WINDOW)
}

/// How a copy is to be run, apart from what it copies.
#[derive(Debug, Clone)]
pub struct Options {
    pub recursive: bool,
    pub preserve: bool,
    pub verbose: bool,
    /// Draw the progress meter, if stderr is a terminal.
    pub meter: bool,
    /// Report skips and failures on stderr. In a local copy the sender
    /// does, the receiver telling it what failed; only one end reports.
    pub report: bool,
    /// File headers sent ahead of their data.
    pub window: usize,
}

impl Options {
    pub fn new(recursive: bool, preserve: bool) -> Options {
        Options {
            recursive,
            preserve,
            verbose: false,
            meter: false,
            report: true,
            window: window(sys::env_or("ACS_CP_WINDOW", WINDOW)),
        }
    }
}

/// How one endpoint's copy went.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Files written and renamed into place.
    pub written: u64,
    /// Files the receiver could not write.
    pub failed: u64,
    /// Entries the sender could not read, or did not copy.
    pub skipped: u64,
    /// Bytes of file data sent or written.
    pub bytes: u64,
    /// What ended the copy early, if anything did.
    pub error: Option<String>,
}

impl Outcome {
    /// The exit status `scp` would give: 0 when every file was written,
    /// 1 when any was skipped or failed or the copy broke off.
    pub fn status(&self) -> u8 {
        match self.error.is_none() && self.failed == 0 && self.skipped == 0 {
            true => code::DETACHED,
            false => code::ERROR,
        }
    }

    /// Count `o`, one part of a copy run in parts, into this one; false
    /// when it broke off, which ends the copy.
    fn add(&mut self, o: Outcome) -> bool {
        self.written += o.written;
        self.failed += o.failed;
        self.skipped += o.skipped;
        self.bytes += o.bytes;
        self.error = o.error;
        self.error.is_none()
    }
}

/// What stops an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// The far end broke the protocol; it is told with an `ERROR`.
    Protocol(String),
    /// The far end sent an `ERROR`, or this end already did.
    Ended(String),
    /// The link went before the copy was done: a new one may finish it.
    Lost(String),
}

/// Why [`drive`] returned before the copy was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The copy itself ended: an `ERROR`, a protocol error.
    Fatal(String),
    /// The link went; the copy may go on over another (ACSCP_DESIGN §7).
    Lost(String),
}

impl Stop {
    fn message(self) -> String {
        match self {
            Stop::Fatal(m) | Stop::Lost(m) => m,
        }
    }
}

/// How much of a part, or of the source it is checked against, is hashed
/// in one turn of the poll loop (ACSCP_DESIGN §7, decision 12): a long
/// `HAVE` never keeps the pings from being answered.
const SLICE: u64 = 1024 * 1024;

/// How long one turn's slice may run, however little of [`SLICE`] it has
/// hashed by then (acs-qeqj). A megabyte is a few milliseconds in a release
/// build, but a third of a second in a debug build on a loaded machine, and
/// a turn that long costs the far end two of them before it hears a `PONG`:
/// the `PING` read in one turn was answered only after the next turn's
/// slice (now flushed straight after the read, acs-wo8h). With the tests'
/// 800 ms dead interval that ran out with both ends alive. Bounded by time, a turn is short on any machine.
const SLICE_TIME: Duration = Duration::from_millis(20);

/// How often, in ms, an endpoint that is [`Endpoint::waiting`] tries again.
const WAIT_STEP: u64 = 100;

/// The first `len` bytes of a file, hashed a [`SLICE`] at a time from the
/// file's current position, which ends at `len`.
pub(crate) struct Prefix {
    pub len: u64,
    left: u64,
    hash: Sha256,
}

impl Prefix {
    pub fn new(len: u64) -> Prefix {
        Prefix {
            len,
            left: len,
            hash: Sha256::new(),
        }
    }

    /// Hash the next slice from `f`: the digest once all `len` bytes are
    /// in, `UnexpectedEof` if the file is shorter.
    pub fn step(&mut self, f: &mut File) -> std::io::Result<Option<[u8; 32]>> {
        self.step_within(f, SLICE_TIME)
    }

    /// [`Prefix::step`], the slice ending at [`SLICE`] bytes or once
    /// `budget` has passed, whichever comes first; always at least one read.
    fn step_within(&mut self, f: &mut File, budget: Duration) -> std::io::Result<Option<[u8; 32]>> {
        let began = Instant::now();
        let mut buf = [0u8; 64 * 1024];
        let mut slice = self.left.min(SLICE);
        let mut first = true;
        while slice > 0 {
            if !first && began.elapsed() >= budget {
                break;
            }
            first = false;
            let want = slice.min(buf.len() as u64) as usize;
            let n = f.read(&mut buf[..want])?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.hash.update(&buf[..n]);
            slice -= n as u64;
            self.left -= n as u64;
        }
        Ok((self.left == 0).then(|| std::mem::take(&mut self.hash).finish()))
    }
}

/// One endpoint's state machine, as [`drive`] runs it.
pub(crate) trait Endpoint {
    /// Queue whatever can be sent now, while `out` holds less than
    /// [`HIGH`].
    fn pump(&mut self, out: &mut OutQueue) -> Result<(), Halt>;
    /// One message from the far end.
    fn on_msg(&mut self, m: Msg<'_>, out: &mut OutQueue) -> Result<(), Halt>;
    /// The far end closed the link.
    fn on_eof(&mut self) -> Result<(), Halt>;
    /// The copy is over for this end, once `out` is written.
    fn finished(&self) -> bool;
    /// The far end's `WELCOME` came, or this end sent its own.
    fn welcomed(&self) -> bool;
    /// Work of its own before the next poll may wait: a part being hashed.
    fn busy(&self) -> bool {
        false
    }
    /// Waiting on something no descriptor reports: a part another copy
    /// holds the lock of. The poll wakes every [`WAIT_STEP`] to try again.
    fn waiting(&self) -> bool {
        false
    }
    /// A new link replaces a lost one, to a new far end that knows nothing
    /// of the old: forget what was in flight on it (ACSCP_DESIGN §7).
    fn reconnect(&mut self) {}
}

/// Note `msg` on stderr as the copy's own, made safe to print: it may hold
/// a name the far end chose (ACSCP_DESIGN §5.2).
pub fn note(msg: &str) {
    use std::io::Write;
    let line = crate::safe::display_max(msg, 4 * crate::safe::MAX_FIELD);
    let _ = writeln!(std::io::stderr(), "{NAME}: {line}");
}

/// [`note`] `what: why`, the shape of every skip and failure reported:
/// one call rather than a `format!` at each place that reports.
pub fn note_on(what: &str, why: &str) {
    note(&[what, ": ", why].concat());
}

/// Hand `ep` every whole message `dec` holds. `PING` and `PONG` are the
/// link's, not the endpoint's: answered and heard here.
fn take_msgs(
    ep: &mut dyn Endpoint,
    dec: &mut Decoder,
    out: &mut OutQueue,
    live: &mut Liveness,
) -> Result<(), Halt> {
    while let Some(m) = Msg::next(dec).map_err(|e| Halt::Protocol(e.to_string()))? {
        match m {
            Msg::Ping(n) => Msg::Pong(n).encode(out.tail()),
            Msg::Pong(n) => live.pong(n),
            m => ep.on_msg(m, out)?,
        }
    }
    Ok(())
}

/// Take one read's `bytes`: hand `ep` what they complete, and tell `live`
/// what it cost. Handling a message can block on this end's own disk, and
/// that is not the far end's silence (acs-wufw, as acs-spd is for the
/// client's terminal): the time is told to [`Liveness::wrote_for`], so a
/// netcheck's answer, unread in the pipe meanwhile, is not given up on, and
/// the read counts as heard once it is handled rather than before, so the
/// dead interval does not age over the stall either.
fn took_read(
    ep: &mut dyn Endpoint,
    dec: &mut Decoder,
    bytes: &[u8],
    out: &mut OutQueue,
    live: &mut Liveness,
) -> Result<(), Halt> {
    dec.push(bytes);
    let began = sys::now_ms();
    let res = take_msgs(ep, dec, out, live);
    live.wrote_for(sys::now_ms().saturating_sub(began));
    live.heard();
    res
}

/// How long the protocol-error flush may take before it gives up.
const ERROR_FLUSH: Duration = Duration::from_secs(2);

/// Write `buf` to the nonblocking `fd`, waiting for room with `poll` but
/// giving up with `TimedOut` once `limit` has passed: a dead link with a full
/// channel never drains, and the flush must not hang the process.
fn write_bounded(fd: RawFd, mut buf: &[u8], limit: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + limit;
    while !buf.is_empty() {
        match sys::write(fd, buf) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                let mut p = [sys::pollfd(fd, libc::POLLOUT)];
                sys::poll(&mut p, sys::poll_timeout(deadline - now))?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Run `ep` over the link `rfd`/`wfd` (one socket, or a pipe each way)
/// until it is finished or stops: `dec` holds what was read already (what
/// came after the remote's marker), and `out` what is still to be written
/// (a `HELLO`). The link's liveness is the session's (ACSCP_DESIGN §5.2):
/// a ping after silence, lost after longer; with `netwatch`, a change of
/// this machine's network asks the far end at once.
pub(crate) fn drive(
    ep: &mut dyn Endpoint,
    rfd: RawFd,
    wfd: RawFd,
    mut dec: Decoder,
    mut out: OutQueue,
    netwatch: Option<&NetWatch>,
) -> Result<(), Stop> {
    let nonblocking = |fd| sys::set_nonblocking(fd, true).map_err(|e| Stop::Fatal(e.to_string()));
    nonblocking(rfd)?;
    nonblocking(wfd)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut eof = false;
    let mut live = Liveness::new();
    let lost = |m: &str| Err(Stop::Lost(m.into()));
    let fail = |h: Halt, out: &mut OutQueue| -> Stop {
        match h {
            Halt::Protocol(m) => {
                Msg::Error {
                    code: proto::err::PROTOCOL,
                    message: m.clone(),
                }
                .encode(out.tail());
                // Best effort: the far end is broken, and may not read, so
                // the flush gives up after `ERROR_FLUSH`.
                let _ = write_bounded(wfd, out.queued(), ERROR_FLUSH);
                Stop::Fatal(format!("protocol error: {m}"))
            }
            Halt::Ended(m) => {
                let _ = out.flush(wfd);
                Stop::Fatal(m)
            }
            Halt::Lost(m) => Stop::Lost(m),
        }
    };
    if let Err(h) = take_msgs(ep, &mut dec, &mut out, &mut live) {
        return Err(fail(h, &mut out));
    }
    if !out.flush(wfd) {
        return lost("the link closed before the copy finished");
    }
    loop {
        let room = out.len() < HIGH;
        if room {
            if let Err(h) = ep.pump(&mut out) {
                return Err(fail(h, &mut out));
            }
        }
        if ep.finished() && out.is_empty() {
            return Ok(());
        }
        if eof && out.is_empty() {
            // Nothing to read and nothing to write: only pump could move,
            // and it did not.
            return lost("the link closed before the copy finished");
        }
        let mut fds = [
            sys::wait_for(rfd, if eof { 0 } else { libc::POLLIN }),
            sys::wait_for(wfd, if out.is_empty() { 0 } else { libc::POLLOUT }),
            sys::pollfd(netwatch.map_or(-1, |w| w.fd()), libc::POLLIN),
        ];
        let mut wait = match room && ep.busy() {
            true => 0,
            false => live.next_deadline_ms().saturating_sub(sys::now_ms()),
        };
        if ep.waiting() {
            wait = wait.min(WAIT_STEP);
        }
        sys::poll(&mut fds, sys::poll_timeout(Duration::from_millis(wait)))
            .map_err(|e| Stop::Fatal(e.to_string()))?;
        if fds[0].revents != 0 {
            match sys::read(rfd, &mut buf) {
                Ok(0) => {
                    eof = true;
                    if let Err(h) = ep.on_eof() {
                        return Err(fail(h, &mut out));
                    }
                }
                Ok(n) => {
                    if let Err(h) = took_read(ep, &mut dec, &buf[..n], &mut out, &mut live) {
                        return Err(fail(h, &mut out));
                    }
                    // What the read asked for goes out now, a `PONG` before
                    // the next turn's work rather than after it (acs-wo8h).
                    if !out.flush(wfd) {
                        return lost("the link closed before the copy finished");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return lost(&format!("the link failed: {e}")),
            }
        }
        if fds[1].revents != 0 && !out.flush(wfd) {
            return lost("the link closed before the copy finished");
        }
        // A network change while the link is up asks the far end at once;
        // one still owed its WELCOME is dialled again if the change took a
        // network away (the session's rules, acs-ft1, acs-xo1).
        if fds[2].revents != 0 {
            if let Some(change) = netwatch.and_then(|w| w.changed()) {
                if ep.welcomed() {
                    live.netcheck(out.tail());
                    change.acted();
                } else if change.removed() {
                    change.acted();
                    return lost("the network moved");
                }
            }
        }
        if !eof && live.tick(out.tail()) == Health::Dead {
            return lost("no answer from the far end");
        }
    }
}

/// Send `sources`, each under its name, over `link` to a receiver that is
/// to write them to `target` (ACSCP_DESIGN §5.2): the `HELLO` first, then
/// the stream.
pub fn send(
    link: &UnixStream,
    target: &str,
    sources: Vec<(String, PathBuf)>,
    opts: &Options,
) -> Outcome {
    let fd = link.as_raw_fd();
    let mut out = OutQueue::default();
    *out.tail() = hello(Role::Receive, target, names(&sources), opts);
    let mut s = sender::Sender::new(sources, opts, false);
    let r = drive(&mut s, fd, fd, Decoder::new(), out, None);
    s.outcome(r.err().map(Stop::message))
}

/// Serve as the receiver over `link`: read the `HELLO`, answer it, and
/// write what arrives.
pub fn receive(link: &UnixStream, opts: &Options) -> Outcome {
    let fd = link.as_raw_fd();
    let mut r = receiver::Receiver::serving(opts);
    let res = drive(&mut r, fd, fd, Decoder::new(), OutQueue::default(), None);
    r.outcome(res.err().map(Stop::message))
}

/// The names `sources` are copied under, as the top-level names their
/// receiver is to accept: each a pattern matching only itself, so a file
/// called `[x]` is not read as a wildcard (ACSCP_DESIGN §5.4).
fn names(sources: &[(String, PathBuf)]) -> Vec<String> {
    sources.iter().map(|(n, _)| glob::literal(n)).collect()
}

/// The `HELLO` that asks the far end to be `role` (ACSCP_DESIGN §5.2): a
/// receiver writing to `target` and accepting `paths` as its top-level
/// names, or a sender of `paths`.
pub fn hello(role: Role, target: &str, paths: Vec<String>, opts: &Options) -> Vec<u8> {
    let mut v = Vec::new();
    Msg::Hello(proto::Hello {
        version: crate::VERSION.into(),
        role,
        recursive: opts.recursive,
        preserve: opts.preserve,
        verbose: opts.verbose,
        target: target.into(),
        paths,
    })
    .encode(&mut v);
    v
}

/// The name a download of the remote `path` arrives under, which is the
/// one top-level name its receiver accepts for it (ACSCP_DESIGN §5.4):
/// the last component as typed — a pattern, if it is one, that the far
/// end's names must match ([`glob`]). `None` where only the remote could say —
/// the home directory, `..`, `/` — and the request is refused rather than
/// any name the far end chose accepted.
pub fn wanted_name(path: &str) -> Option<String> {
    let rest = match path.strip_prefix('~') {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        _ => path,
    };
    let n = Path::new(rest).file_name()?.to_str()?;
    proto::valid_name(n).then(|| n.to_string())
}

/// One dial of `acs _cp` on the host `conn` names.
enum Dialed {
    /// Up, with what came after the marker.
    Up(crate::client::Link, Vec<u8>),
    /// No connection: worth another try on a redial.
    Down(String),
    /// The host answered and cannot copy: not installed and not to be, an
    /// install that failed, another protocol. Final, with its exit code.
    Refused(String, u8),
}

/// Dial `acs _cp` once (ACSCP_DESIGN §3, §8): the alias resolved again
/// first, `hello` written the moment ssh is spawned, acs installed on the
/// same connection where the host lacks it. The first connection is a
/// `Call::Session`, on acs's ssh master, with the first connection's
/// answer timeout; a `redial` is a `Call::Redial` of its own, with the
/// redial's (§7).
fn dial_cp(
    conn: &mut crate::cli::Connection,
    redial: bool,
    hello: &[u8],
    netwatch: Option<&NetWatch>,
) -> Dialed {
    use crate::client;
    let label = if redial { "redial" } else { "first connection" };
    let mut timing = Timing::start(conn.verbose > 0, label);
    let name = conn
        .alias
        .clone()
        .unwrap_or_else(|| conn.transport.destination.clone());
    if let Err(e) = client::resolve_alias(conn, &name) {
        return Dialed::Down(e);
    }
    if conn.alias.is_some() {
        timing.mark(Phase::AliasResolved);
    }
    let (remote, sync) = client::remote_command(conn, &["_cp"]);
    let again = sync.as_ref().map(|_| hello.to_vec()).unwrap_or_default();
    let (call, timeout) = match redial {
        true => (Call::Redial, client::redial_timeout()),
        false => (Call::Session, client::answer_timeout(false)),
    };
    let first = hello.to_vec();
    let t = &mut timing;
    let (link, marker) =
        match client::dial_watching(conn, call, &remote, timeout, t, first, netwatch, None) {
            Ok(x) => x,
            // A knock that cannot work as configured is final (KNOCK_DESIGN §8).
            Err(e) => match client::knock_refusal(&e) {
                Some(crate::knock::Refusal::Config(m)) => {
                    return Dialed::Refused(m.clone(), crate::client::code::ERROR)
                }
                _ => return Dialed::Down(e.to_string()),
            },
        };
    match client::ready_or_install(conn, link, marker, sync, &again, t) {
        Ok((link, rest)) => Dialed::Up(link, rest),
        Err((e, c)) => Dialed::Refused(e, c),
    }
}

/// Sit out `wait` before the next dial, cut short by a change of this
/// machine's network at most once per `ACS_EARLY_MS`, as the session's
/// offline wait is. True when it was.
fn sit_out(wait: Duration, netwatch: Option<&NetWatch>, last_early: &mut Option<Instant>) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let mut fds = [sys::pollfd(netwatch.map_or(-1, |w| w.fd()), libc::POLLIN)];
        let _ = sys::poll(&mut fds, sys::poll_timeout(deadline - now));
        if fds[0].revents == 0 {
            continue;
        }
        if let Some(change) = netwatch.and_then(|w| w.changed()) {
            let every = crate::reconnect::early_every();
            if last_early.map_or(true, |t| t.elapsed() >= every) {
                change.acted();
                *last_early = Some(Instant::now());
                return true;
            }
        }
    }
}

/// What reaching `host` takes, with the configuration loaded and acs's
/// ssh master decided; the exit code, told already, for a configuration
/// that does not load.
fn prepare(a: &CopyArgs, host: &cli::Remote) -> Result<crate::cli::Connection, u8> {
    use crate::client::note;
    let mut conn = a.connection(host);
    conn.config = crate::config::Config::load().map_err(|e| {
        note(&e.to_string());
        code::USAGE
    })?;
    crate::update_check::on_client_start(&conn.config);
    // No local forwards: a copy forwards nothing, and a forward would cost
    // it the shared master (ACSCP_DESIGN §4.1).
    let decided = crate::mux::configure(&mut conn.transport);
    if conn.verbose > 0 {
        note(&decided);
    }
    Ok(conn)
}

/// Run `ep` against `acs _cp` on `host` over as many links as it takes
/// (ACSCP_DESIGN §7): a lost link is redialled at once and then on the
/// session's backoff, cut short by a network change, until the copy is
/// done or interrupted — or, with `--no-reconnect`, not at all. Each new
/// link starts from what is on disk: the sender leaves out what was
/// placed, and a part is resumed by `HAVE`. `Err` is the exit code of a
/// copy whose first connection never came up, told already; `Ok` is how
/// the copy itself went.
fn over_ssh(
    a: &CopyArgs,
    host: &cli::Remote,
    hello: Vec<u8>,
    ep: &mut dyn Endpoint,
) -> Result<Result<(), String>, u8> {
    use crate::client::note;
    let mut conn = prepare(a, host)?;
    let netwatch = match a.reconnect {
        true => NetWatch::new(conn.verbose > 0),
        false => None,
    };
    let mut backoff = Backoff::new(sys::env_or("ACS_BACKOFF_MS", 1000));
    let mut last_early = None;
    let mut redial = false;
    loop {
        let why = match dial_cp(&mut conn, redial, &hello, netwatch.as_ref()) {
            Dialed::Up(mut link, rest) => {
                if redial {
                    ep.reconnect();
                }
                let mut dec = Decoder::new();
                dec.push(&rest);
                let mut out = OutQueue::default();
                *out.tail() = link.take_pending();
                let (r, w) = (link.from_fd().as_raw_fd(), link.to_fd().as_raw_fd());
                let up = Instant::now();
                let res = drive(ep, r, w, dec, out, netwatch.as_ref());
                link.close();
                match res {
                    Ok(()) => return Ok(Ok(())),
                    Err(Stop::Fatal(m)) => return Ok(Err(m)),
                    Err(Stop::Lost(m)) => {
                        let served = ep.welcomed().then(|| up.elapsed());
                        backoff.link_ended(served, crate::reconnect::backoff_reset_after());
                        m
                    }
                }
            }
            Dialed::Down(e) if !redial => {
                note(&e);
                return Err(code::UNREACHABLE);
            }
            Dialed::Refused(e, c) if !redial => {
                note(&e);
                return Err(c);
            }
            Dialed::Refused(e, _) => return Ok(Err(e)),
            Dialed::Down(e) => e,
        };
        if !a.reconnect {
            return Ok(Err(why));
        }
        redial = true;
        // On stderr as a line of its own: nothing of the copy's is on the
        // terminal to protect, and Ctrl-C ends it with the part kept.
        let wait = backoff.step();
        let dest = host.destination();
        self::note(&match wait.is_zero() {
            true => format!("{dest}: {why} — reconnecting now"),
            false => format!("{dest}: {why} — reconnecting in {}s", wait.as_secs().max(1)),
        });
        if sit_out(wait, netwatch.as_ref(), &mut last_early) {
            backoff.spent();
        }
    }
}

/// One source host's paths and the names they arrive under.
type Group<'a> = (&'a cli::Remote, Vec<String>, Vec<String>);

/// The command line's remote sources by host, in the order it first names
/// each; the exit code, told already, for a path that names no file.
fn groups(a: &CopyArgs) -> Result<Vec<Group<'_>>, u8> {
    let mut groups: Vec<Group<'_>> = Vec::new();
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
    Ok(groups)
}

/// A copy with a host in it (ACSCP_DESIGN §5.6, DESIGN decision 17), run
/// in parts: first each source host's sources in turn — a download into a
/// local target, a relay into a remote one — then the local sources, copied
/// or uploaded. Several sources, or a wildcard, need a directory: a local
/// target is looked at before anything is dialled, and a remote one by the
/// first part, before anything is written, which is why the hosts go first.
fn copy_remote(a: &CopyArgs, opts: &Options) -> Result<Outcome, u8> {
    let groups = groups(a)?;
    let (good, skipped) = local_sources(a);
    let several = a.sources.len() > 1
        || groups
            .iter()
            .any(|g| g.1.iter().any(|p| glob::is_pattern(p)));
    let mut total = Outcome {
        skipped,
        ..Outcome::default()
    };
    let more = match &a.target {
        Spec::Local(target) => {
            if several && !target.is_dir() {
                note_on(&target.display().to_string(), "not a directory");
                return Err(code::ERROR);
            }
            let mut more = true;
            for (host, paths, wanted) in groups {
                let hello = hello(Role::Send, "", paths, opts);
                let mut r = receiver::Receiver::asking(target.clone(), wanted, opts);
                let res = over_ssh(a, host, hello, &mut r)?;
                more = total.add(r.outcome(res.err()));
                if !more {
                    break;
                }
            }
            more
        }
        Spec::Remote(_) if groups.is_empty() => true,
        Spec::Remote(dst) => total.add(relay::copy(a, dst, groups, several, opts)?),
    };
    if !more || good.is_empty() {
        return Ok(total);
    }
    let o = match &a.target {
        Spec::Local(target) => copy_local(target, good, opts),
        Spec::Remote(host) => {
            let hello = hello(Role::Receive, &host.path, names(&good), opts);
            let mut s = sender::Sender::new(good, opts, false);
            let r = over_ssh(a, host, hello, &mut s)?;
            s.outcome(r.err())
        }
    };
    total.add(o);
    Ok(total)
}

/// A local copy: the sender here and a receiver on a thread of its own,
/// over a socketpair (ACSCP_DESIGN §10.2). The sender reports; the
/// receiver tells it what failed.
pub fn copy_local(target: &Path, sources: Vec<(String, PathBuf)>, opts: &Options) -> Outcome {
    let (a, b) = match UnixStream::pair() {
        Ok(p) => p,
        Err(e) => {
            return Outcome {
                error: Some(format!("socketpair: {e}")),
                ..Outcome::default()
            }
        }
    };
    let mut ropts = opts.clone();
    ropts.report = false;
    ropts.meter = false;
    let rx = std::thread::spawn(move || {
        let o = receive(&b, &ropts);
        drop(b);
        o
    });
    let target = target.to_string_lossy();
    let mut o = send(&a, &target, sources, opts);
    drop(a);
    let r = rx.join().unwrap_or_default();
    o.bytes = o.bytes.max(r.bytes);
    o
}

/// The checks before anything is dialled (ACSCP_DESIGN §4.2): each local
/// source exists and can be read, and is a directory only with `-r`. What
/// fails is reported and skipped; what passes comes back with the name it
/// is copied under.
pub fn check_sources(sources: &[PathBuf], recursive: bool) -> (Vec<(String, PathBuf)>, u64) {
    let mut good = Vec::new();
    let mut skipped = 0;
    for path in sources {
        match walk::check_source(path, recursive) {
            Ok(name) => good.push((name, path.clone())),
            Err(reason) => {
                note_on(&path.display().to_string(), &reason);
                skipped += 1;
            }
        }
    }
    (good, skipped)
}

/// The command line's local sources, [checked](check_sources): those that
/// passed and how many did not.
fn local_sources(a: &CopyArgs) -> (Vec<(String, PathBuf)>, u64) {
    let local: Vec<PathBuf> = a
        .sources
        .iter()
        .filter_map(|s| match s {
            Spec::Local(p) => Some(p.clone()),
            Spec::Remote(_) => None,
        })
        .collect();
    check_sources(&local, a.recursive)
}

/// Entry point of the copy role: `args` after the program name (`acscp`)
/// or after `cp` (`acs cp`).
pub fn main(args: &[OsString]) -> ExitCode {
    let a = match cli::parse(args.iter().cloned()) {
        Ok(Parsed::Run(a)) => a,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            crate::print_version();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            note(&e);
            eprintln!(
                "{}",
                cli::USAGE.lines().take(2).collect::<Vec<_>>().join("\n")
            );
            return ExitCode::from(code::USAGE);
        }
    };
    let started = Instant::now();
    let mut opts = Options::new(a.recursive, a.preserve);
    opts.verbose = a.verbose > 0;
    opts.meter = !a.quiet;
    let o = match (&a.target, a.any_remote()) {
        (Spec::Local(target), false) => match local_sources(&a) {
            (good, _) if good.is_empty() => Err(code::ERROR),
            (good, skipped) => {
                let mut o = copy_local(target, good, &opts);
                o.skipped += skipped;
                Ok(o)
            }
        },
        _ => copy_remote(&a, &opts),
    };
    let o = match o {
        Ok(o) => o,
        Err(c) => return ExitCode::from(c),
    };
    if let Some(e) = &o.error {
        note(e);
    }
    if opts.verbose {
        note(&format!(
            "{} written, {} failed, {} skipped, {} in {}",
            o.written,
            o.failed,
            o.skipped,
            progress::bytes(o.bytes),
            progress::tenths(started.elapsed().as_nanos(), 1_000_000_000, "s")
        ));
    }
    ExitCode::from(o.status())
}

#[cfg(test)]
mod flush_tests {
    use super::*;
    use std::sync::mpsc;

    /// A reader that never reads must not hold the protocol-error flush
    /// past its deadline (acs-6a73). The call runs on a thread so that the
    /// unfixed, blocking flush fails the test instead of hanging the suite.
    #[test]
    fn bounded_write_gives_up_on_a_reader_that_never_reads() {
        let (a, _b) = UnixStream::pair().unwrap();
        sys::set_nonblocking(a.as_raw_fd(), true).unwrap();
        let fd = a.as_raw_fd();
        let big = vec![0u8; 16 * 1024 * 1024];
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(write_bounded(fd, &big, Duration::from_millis(200)));
        });
        let r = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the flush hung on a full link");
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn bounded_write_delivers_what_fits() {
        let (a, mut b) = UnixStream::pair().unwrap();
        sys::set_nonblocking(a.as_raw_fd(), true).unwrap();
        write_bounded(a.as_raw_fd(), b"hello", Duration::from_secs(1)).unwrap();
        let mut got = [0u8; 5];
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
    }
}

#[cfg(test)]
mod liveness_tests {
    use super::*;

    /// An endpoint whose every message takes `stall` to handle: a disk
    /// that is slow to write.
    struct Slow {
        stall: Duration,
    }

    impl Endpoint for Slow {
        fn pump(&mut self, _: &mut OutQueue) -> Result<(), Halt> {
            Ok(())
        }
        fn on_msg(&mut self, _: Msg<'_>, _: &mut OutQueue) -> Result<(), Halt> {
            std::thread::sleep(self.stall);
            Ok(())
        }
        fn on_eof(&mut self) -> Result<(), Halt> {
            Ok(())
        }
        fn finished(&self) -> bool {
            false
        }
        fn welcomed(&self) -> bool {
            true
        }
    }

    /// acs-wufw: a message that takes the local disk longer than
    /// `ACS_NETCHECK_MS` to write, handled while a netcheck is out, is not
    /// the far end's silence: its `PONG` is still unread in the pipe, and
    /// the link is not declared dead for it.
    #[test]
    fn a_slow_disk_does_not_fail_an_outstanding_netcheck() {
        let mut live = Liveness::new();
        let mut asked = Vec::new();
        live.netcheck(&mut asked);
        let mut msg = Vec::new();
        Msg::DirEnd.encode(&mut msg);
        // Longer than the netcheck's deadline (default 2 s), well short of
        // the dead interval.
        let mut ep = Slow {
            stall: Duration::from_millis(sys::env_or("ACS_NETCHECK_MS", 2000u64) + 300),
        };
        let mut out = OutQueue::default();
        let mut dec = Decoder::new();
        took_read(&mut ep, &mut dec, &msg, &mut out, &mut live).unwrap();
        let mut ping = Vec::new();
        assert_eq!(live.tick(&mut ping), Health::Ok);
    }

    /// An endpoint whose every `pump` is slow work, as a turn of hashing is.
    struct Busy;

    impl Endpoint for Busy {
        fn pump(&mut self, _: &mut OutQueue) -> Result<(), Halt> {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        }
        fn on_msg(&mut self, _: Msg<'_>, _: &mut OutQueue) -> Result<(), Halt> {
            Ok(())
        }
        fn on_eof(&mut self) -> Result<(), Halt> {
            Err(Halt::Ended("done".into()))
        }
        fn finished(&self) -> bool {
            false
        }
        fn welcomed(&self) -> bool {
            true
        }
        fn busy(&self) -> bool {
            true
        }
    }

    /// acs-wo8h: a `PING` read in a turn is answered before that turn's
    /// work, not after the next turn's slice: the far end waits for the
    /// `PONG` however long a turn runs.
    #[test]
    fn a_ping_is_answered_before_the_next_turns_work() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            // Let the first turn's work begin, so the PING lands in a
            // turn that has more work queued behind it.
            std::thread::sleep(Duration::from_millis(100));
            let mut ping = Vec::new();
            Msg::Ping(7).encode(&mut ping);
            std::io::Write::write_all(&mut b, &ping).unwrap();
            let sent = Instant::now();
            let mut dec = Decoder::new();
            let mut buf = [0u8; 64];
            let answered = loop {
                let n = b.read(&mut buf).unwrap();
                assert!(n > 0, "closed before the PONG");
                dec.push(&buf[..n]);
                if let Some(Msg::Pong(7)) = Msg::next(&mut dec).unwrap() {
                    break sent.elapsed();
                }
            };
            answered
        });
        let fd = a.as_raw_fd();
        let r = drive(&mut Busy, fd, fd, Decoder::new(), OutQueue::default(), None);
        drop(a);
        assert!(r.is_err() || peer.is_finished());
        let answered = peer.join().unwrap();
        assert!(
            // The turn under way (500 ms, sent 100 ms in) ends first; the
            // next one's 500 ms must not come before the answer.
            answered < Duration::from_millis(700),
            "the PONG took {answered:?}, a turn's work first"
        );
    }

    /// `ACS_CP_WINDOW` is kept under the descriptor limit: each header
    /// ahead holds its source open, so a window of thousands failed files
    /// with "Too many open files" (acs-d2kt).
    #[test]
    fn the_window_is_clamped() {
        assert_eq!(window(0), 1);
        assert_eq!(window(WINDOW), WINDOW);
        assert_eq!(window(100_000), MAX_WINDOW);
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A slice stops once its time is up, not only at [`SLICE`] bytes
    /// (acs-qeqj): a megabyte per turn took a third of a second in a debug
    /// build under load, and the far end, owed a `PONG` two turns later,
    /// gave the live link up. With no time at all a turn is one read.
    #[test]
    fn a_slice_ends_when_its_time_is_up() {
        let t = TempDir::new();
        let path = t.path().join("f");
        let body: Vec<u8> = (0..2 * SLICE + 10).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let len = body.len() as u64;
        let mut f = File::open(&path).unwrap();
        let mut p = Prefix::new(len);

        assert_eq!(p.step_within(&mut f, Duration::ZERO).unwrap(), None);
        assert_eq!(p.left, len - 64 * 1024, "one read, not a whole slice");
        assert_eq!(p.step_within(&mut f, Duration::MAX).unwrap(), None);
        assert_eq!(p.left, len - 64 * 1024 - SLICE, "never past a slice");
        let mut turns = 2;
        let hash = loop {
            turns += 1;
            if let Some(h) = p.step_within(&mut f, Duration::ZERO).unwrap() {
                break h;
            }
        };
        assert_eq!(hash, crate::sha256::digest(&body), "the same digest");
        assert!(turns > 3, "{turns} turns");
    }
}
