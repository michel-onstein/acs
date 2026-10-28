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
//! Built so far: phases 1 and 2 of the design's plan (§12) — the
//! protocol, the two endpoints, local copies, and uploads and downloads
//! with one host. A drop ends the copy with the part kept until phase 3
//! resumes it; two hosts (the relay) are phase 5.

pub mod cli;
pub mod progress;
pub mod proto;
pub mod receiver;
pub mod remote;
pub mod sender;
pub mod walk;

use std::ffi::OsString;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use crate::client::code;
use crate::outq::OutQueue;
use crate::proto::Decoder;
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
            window: sys::env_or("ACS_CP_WINDOW", WINDOW).max(1),
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
}

/// What stops an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// The far end broke the protocol; it is told with an `ERROR`.
    Protocol(String),
    /// The far end sent an `ERROR`, or this end already did.
    Ended(String),
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

/// Hand `ep` every whole message `dec` holds.
fn take_msgs(ep: &mut dyn Endpoint, dec: &mut Decoder, out: &mut OutQueue) -> Result<(), Halt> {
    while let Some(m) = Msg::next(dec).map_err(|e| Halt::Protocol(e.to_string()))? {
        ep.on_msg(m, out)?;
    }
    Ok(())
}

/// Run `ep` over the link `rfd`/`wfd` (one socket, or a pipe each way)
/// until it is finished or stops: `dec` holds what was read already (what
/// came after the remote's marker), and `out` what is still to be written
/// (a `HELLO`).
pub(crate) fn drive(
    ep: &mut dyn Endpoint,
    rfd: RawFd,
    wfd: RawFd,
    mut dec: Decoder,
    mut out: OutQueue,
) -> Result<(), String> {
    let nonblocking = |fd| sys::set_nonblocking(fd, true).map_err(|e| e.to_string());
    nonblocking(rfd)?;
    nonblocking(wfd)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut eof = false;
    let fail = |h: Halt, out: &mut OutQueue| -> String {
        match h {
            Halt::Protocol(m) => {
                Msg::Error {
                    code: proto::err::PROTOCOL,
                    message: m.clone(),
                }
                .encode(out.tail());
                // Best effort: the far end is broken, and may not read.
                let _ = sys::set_nonblocking(wfd, false);
                let _ = sys::write_all(wfd, out.queued());
                format!("protocol error: {m}")
            }
            Halt::Ended(m) => {
                let _ = out.flush(wfd);
                m
            }
        }
    };
    if let Err(h) = take_msgs(ep, &mut dec, &mut out) {
        return Err(fail(h, &mut out));
    }
    loop {
        if out.len() < HIGH {
            if let Err(h) = ep.pump(&mut out) {
                return Err(fail(h, &mut out));
            }
        }
        if ep.finished() && out.is_empty() {
            return Ok(());
        }
        let mut fds = [
            sys::wait_for(rfd, if eof { 0 } else { libc::POLLIN }),
            sys::wait_for(wfd, if out.is_empty() { 0 } else { libc::POLLOUT }),
        ];
        if eof && out.is_empty() {
            // Nothing to read and nothing to write: only pump could move,
            // and it did not.
            return Err(fail(
                Halt::Ended("the link closed before the copy finished".into()),
                &mut out,
            ));
        }
        sys::poll(&mut fds, -1).map_err(|e| e.to_string())?;
        if fds[0].revents != 0 {
            match sys::read(rfd, &mut buf) {
                Ok(0) => {
                    eof = true;
                    if let Err(h) = ep.on_eof() {
                        return Err(fail(h, &mut out));
                    }
                }
                Ok(n) => {
                    dec.push(&buf[..n]);
                    if let Err(h) = take_msgs(ep, &mut dec, &mut out) {
                        return Err(fail(h, &mut out));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("the link failed: {e}")),
            }
        }
        if fds[1].revents != 0 && !out.flush(wfd) {
            return Err("the link closed before the copy finished".into());
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
    let r = drive(&mut s, fd, fd, Decoder::new(), out);
    s.outcome(r.err())
}

/// Serve as the receiver over `link`: read the `HELLO`, answer it, and
/// write what arrives.
pub fn receive(link: &UnixStream, opts: &Options) -> Outcome {
    let fd = link.as_raw_fd();
    let mut r = receiver::Receiver::serving(opts);
    let res = drive(&mut r, fd, fd, Decoder::new(), OutQueue::default());
    r.outcome(res.err())
}

/// The names `sources` are copied under: the top-level names their
/// receiver is to accept.
fn names(sources: &[(String, PathBuf)]) -> Vec<String> {
    sources.iter().map(|(n, _)| n.clone()).collect()
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
/// the last component as typed. `None` where only the remote could say —
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

/// Run `ep` against `acs _cp` on `host` (ACSCP_DESIGN §3, §8): the
/// session's dial — a `Call::Session` on acs's ssh master, the alias
/// resolved as a session resolves it — with `hello` written the moment ssh
/// is spawned, and acs installed on the same connection where the host
/// lacks it. `Err` is the exit code of a copy that never started, told
/// already; `Ok` is how the copy itself went.
fn over_ssh(
    a: &CopyArgs,
    host: &cli::Remote,
    hello: Vec<u8>,
    ep: &mut dyn Endpoint,
) -> Result<Result<(), String>, u8> {
    use crate::client::{self, note};
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
    let mut timing = Timing::start(conn.verbose > 0, "first connection");
    let name = conn.transport.destination.clone();
    client::resolve_alias(&mut conn, &name).map_err(|e| {
        note(&e);
        code::UNREACHABLE
    })?;
    if conn.alias.is_some() {
        timing.mark(Phase::AliasResolved);
    }
    let (remote, sync) = client::remote_command(&conn, &["_cp"]);
    let again = sync.as_ref().map(|_| hello.clone()).unwrap_or_default();
    let timeout = client::answer_timeout(false);
    let dialed = client::dial(&conn, Call::Session, &remote, timeout, &mut timing, hello);
    let (link, marker) = dialed.map_err(|e| {
        note(&e.to_string());
        code::UNREACHABLE
    })?;
    let (mut link, rest) = client::ready_or_install(&conn, link, marker, sync, &again, &mut timing)
        .map_err(|(e, c)| {
            note(&e);
            c
        })?;
    let mut dec = Decoder::new();
    dec.push(&rest);
    let mut out = OutQueue::default();
    *out.tail() = link.take_pending();
    let (r, w) = (link.from_fd().as_raw_fd(), link.to_fd().as_raw_fd());
    let res = drive(ep, r, w, dec, out);
    link.close();
    Ok(res)
}

/// A copy to or from one host: an upload of local sources, or a download
/// of sources all on the same `[user@]host`. Anything else — two hosts, or
/// local and remote sources together — is the relay of phase 5
/// (ACSCP_DESIGN §5.6) and refused.
fn copy_remote(a: &CopyArgs, opts: &Options) -> Result<Outcome, u8> {
    let mut hosts = a.sources.iter().filter_map(|s| match s {
        Spec::Remote(r) => Some(r),
        Spec::Local(_) => None,
    });
    let first = hosts.next();
    let one_host = first.is_some_and(|f| {
        hosts.all(|r| r.destination() == f.destination())
            && a.sources.iter().all(|s| matches!(s, Spec::Remote(_)))
    });
    match (&a.target, first) {
        (Spec::Remote(host), None) => {
            let (good, skipped) = local_sources(a)?;
            let hello = hello(Role::Receive, &host.path, names(&good), opts);
            let mut s = sender::Sender::new(good, opts, false);
            let r = over_ssh(a, host, hello, &mut s)?;
            let mut o = s.outcome(r.err());
            o.skipped += skipped;
            Ok(o)
        }
        (Spec::Local(target), Some(host)) if one_host => {
            let mut paths = Vec::new();
            let mut wanted = Vec::new();
            for s in &a.sources {
                let Spec::Remote(r) = s else { continue };
                let Some(n) = wanted_name(&r.path) else {
                    note(&format!(
                        "{}:{}: name the file or directory to copy, not the one it is in",
                        r.destination(),
                        r.path
                    ));
                    return Err(code::USAGE);
                };
                paths.push(r.path.clone());
                wanted.push(n);
            }
            if paths.len() > 1 && !target.is_dir() {
                note_on(&target.display().to_string(), "not a directory");
                return Err(code::ERROR);
            }
            let hello = hello(Role::Send, "", paths, opts);
            let mut r = receiver::Receiver::asking(target.clone(), wanted, opts);
            let res = over_ssh(a, host, hello, &mut r)?;
            Ok(r.outcome(res.err()))
        }
        _ => {
            note("copying between two hosts, or from local and remote sources at once, is not built yet");
            Err(code::USAGE)
        }
    }
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
/// passed and how many did not, or the exit code when none passed.
fn local_sources(a: &CopyArgs) -> Result<(Vec<(String, PathBuf)>, u64), u8> {
    let local: Vec<PathBuf> = a
        .sources
        .iter()
        .filter_map(|s| match s {
            Spec::Local(p) => Some(p.clone()),
            Spec::Remote(_) => None,
        })
        .collect();
    match check_sources(&local, a.recursive) {
        (good, _) if good.is_empty() => Err(code::ERROR),
        checked => Ok(checked),
    }
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
        (Spec::Local(target), false) => local_sources(&a).map(|(good, skipped)| {
            let mut o = copy_local(target, good, &opts);
            o.skipped += skipped;
            o
        }),
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
