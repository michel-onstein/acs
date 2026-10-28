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
//! Built so far: phase 1 of the design's plan (§12) — the protocol, the
//! two endpoints and local copies. A copy to or from a host (`acs _cp`,
//! phase 2) is refused with a note until then.

pub mod cli;
pub mod progress;
pub mod proto;
pub mod receiver;
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
use crate::sys;
use cli::{Parsed, Spec};
use proto::Msg;

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

/// Run `ep` over the link `rfd`/`wfd` (one socket, or a pipe each way)
/// until it is finished or stops.
pub(crate) fn drive(ep: &mut impl Endpoint, rfd: RawFd, wfd: RawFd) -> Result<(), String> {
    let nonblocking = |fd| sys::set_nonblocking(fd, true).map_err(|e| e.to_string());
    nonblocking(rfd)?;
    nonblocking(wfd)?;
    let mut dec = crate::proto::Decoder::new();
    let mut out = OutQueue::default();
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
                    loop {
                        let m = match Msg::next(&mut dec) {
                            Ok(Some(m)) => m,
                            Ok(None) => break,
                            Err(e) => return Err(fail(Halt::Protocol(e.to_string()), &mut out)),
                        };
                        if let Err(h) = ep.on_msg(m, &mut out) {
                            return Err(fail(h, &mut out));
                        }
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
    let mut s = sender::Sender::new(target, sources, opts);
    let r = drive(&mut s, fd, fd);
    s.outcome(r.err())
}

/// Serve as the receiver over `link`: read the `HELLO`, answer it, and
/// write what arrives.
pub fn receive(link: &UnixStream, opts: &Options) -> Outcome {
    let fd = link.as_raw_fd();
    let mut r = receiver::Receiver::serving(opts);
    let res = drive(&mut r, fd, fd);
    r.outcome(res.err())
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
                note(&format!("{}: {reason}", path.display()));
                skipped += 1;
            }
        }
    }
    (good, skipped)
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
    if a.any_remote() {
        note("copying to or from a host is not built yet: only local copies for now");
        return ExitCode::from(code::USAGE);
    }
    let started = Instant::now();
    let sources: Vec<PathBuf> = a
        .sources
        .iter()
        .filter_map(|s| match s {
            Spec::Local(p) => Some(p.clone()),
            Spec::Remote(_) => None,
        })
        .collect();
    let Spec::Local(target) = &a.target else {
        unreachable!("no remote argument")
    };
    let (good, skipped) = check_sources(&sources, a.recursive);
    if good.is_empty() {
        return ExitCode::from(code::ERROR);
    }
    let mut opts = Options::new(a.recursive, a.preserve);
    opts.verbose = a.verbose > 0;
    opts.meter = !a.quiet;
    let mut o = copy_local(target, good, &opts);
    o.skipped += skipped;
    if let Some(e) = &o.error {
        note(e);
    }
    if opts.verbose {
        note(&format!(
            "{} written, {} failed, {} skipped, {} in {:.1}s",
            o.written,
            o.failed,
            o.skipped,
            progress::bytes(o.bytes),
            started.elapsed().as_secs_f64()
        ));
    }
    ExitCode::from(o.status())
}
