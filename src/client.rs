//! The local client, `acs <host> [session]` (DESIGN §3, §4.4, §5, §6, §7).
//!
//! One connection is a [`Link`]: the ssh (or test transport) child with its
//! pipes. [`serve`] runs the session over a link until the session ends, the
//! user detaches, or the link is lost.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::{Child, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::cli::{self, ClientArgs, Parsed, Target};
use crate::keys::{self, Action, Detector};
use crate::modes::ModeObserver;
use crate::proto::{self, AttachKind, Decoder, Hello, Marker, MarkerScanner, Mode, Msg, Resume};
use crate::resume::Unacked;
use crate::ssh::{self, Call};
use crate::sys;
use crate::tty::{self, RawMode};

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;

/// Exit codes besides the session's own (DESIGN §7).
pub mod code {
    pub const DETACHED: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const USAGE: u8 = 2;
    pub const TAKEN_OVER: u8 = 3;
    pub const NO_SESSION: u8 = 4;
    pub const INSTALL_FAILED: u8 = 254;
    pub const UNREACHABLE: u8 = 255;
}

/// Entry point of the client (arguments after the program name).
pub fn main(args: &[OsString]) -> ExitCode {
    let default = std::env::var("ACS_DEFAULT_SESSION").ok();
    let parsed = match cli::parse(args.iter().cloned(), default.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(code::USAGE);
        }
    };
    let mut args = match parsed {
        Parsed::Help => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Parsed::Version => {
            crate::print_version();
            return ExitCode::SUCCESS;
        }
        Parsed::Run(a) => *a,
    };
    args.config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(code::USAGE);
        }
    };
    let name = args.transport.destination.clone();
    if let Err(e) = resolve_alias(&mut args, &name) {
        eprintln!("acs: {e}");
        return ExitCode::from(code::UNREACHABLE);
    }
    if args.list {
        return crate::list::run(&args);
    }
    ExitCode::from(run(args))
}

/// If `name` is an alias, point the transport at the host it stands for
/// now (DESIGN §7.3); with `-v`, say which entry was chosen and why.
pub fn resolve_alias(args: &mut ClientArgs, name: &str) -> Result<(), String> {
    let verbose = args.verbose > 0;
    let dest = crate::alias::resolve(name, &args.config, &mut crate::alias::ping, &mut |m| {
        if verbose {
            note(&m)
        }
    })?;
    if let Some(d) = dest {
        args.alias = Some(name.to_string());
        args.transport.destination = d;
    }
    Ok(())
}

/// Who we are, for the master's takeover decisions (DESIGN §4.5).
pub fn identity() -> String {
    std::env::var("ACS_IDENTITY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let user = sys::user_name(sys::getuid()).unwrap_or_else(|| "user".into());
            format!("{user}@{}", sys::hostname())
        })
}

fn escape_config() -> keys::Config {
    let mut cfg = keys::Config::default();
    if let Some(k) = std::env::var("ACS_ESCAPE_KEY")
        .ok()
        .and_then(|v| keys::Config::parse_key(&v))
    {
        cfg.byte = k;
    }
    if let Some(ms) = std::env::var("ACS_ESCAPE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        cfg.window_ms = ms;
    }
    cfg
}

/// Print a message on the terminal outside the session's byte stream.
/// Works in raw mode too (explicit `\r\n`).
pub fn note(msg: &str) {
    let _ = sys::write_all(2, format!("acs: {msg}\r\n").as_bytes());
}

// ---- links -----------------------------------------------------------------

/// One connection to the remote: the transport child and its pipes.
pub struct Link {
    child: Child,
    to: OwnedFd,
    from: OwnedFd,
}

impl Link {
    /// The pipe carrying the remote's output.
    pub fn from_fd(&self) -> &OwnedFd {
        &self.from
    }

    /// The pipe carrying our input to the remote.
    pub fn to_fd(&self) -> &OwnedFd {
        &self.to
    }

    pub fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the transport running `remote` and wait for the marker line.
pub fn dial(args: &ClientArgs, call: Call, remote: &str) -> io::Result<(Link, Marker)> {
    let mut cmd = args.transport.command(call, remote);
    if args.verbose > 0 {
        note(&format!(
            "running {}",
            ssh::display_argv(&args.transport.argv(call, remote))
        ));
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| {
        let argv = args.transport.argv(call, remote);
        io::Error::new(
            e.kind(),
            format!("cannot run {}: {e}", argv[0].to_string_lossy()),
        )
    })?;
    let to: OwnedFd = child.stdin.take().unwrap().into();
    let from: OwnedFd = child.stdout.take().unwrap().into();
    let mut scanner = MarkerScanner::new();
    let mut buf = [0u8; 4096];
    let marker = loop {
        let n = sys::read(from.as_raw_fd(), &mut buf)?;
        if n == 0 {
            if args.verbose > 0 && !scanner.noise.is_empty() {
                note(&format!(
                    "remote said: {}",
                    String::from_utf8_lossy(&scanner.noise)
                ));
            }
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed before acs started on the remote",
            ));
        }
        if let Some(m) = scanner.push(&buf[..n]) {
            break m;
        }
    };
    if args.verbose > 0 && !scanner.noise.is_empty() {
        note(&format!(
            "skipped remote login output: {:?}",
            String::from_utf8_lossy(&scanner.noise)
        ));
    }
    Ok((Link { child, to, from }, marker))
}

// ---- session state that survives reconnects --------------------------------

/// What the client knows about its session across links (DESIGN §5.2).
pub struct State {
    pub host: String,
    /// Known once the first WELCOME arrives (not before, for `--new`).
    pub session: Option<String>,
    pub instance: Option<u64>,
    /// Offset of the next output byte we expect.
    pub offset: u64,
    pub unacked: Unacked,
    pub observer: ModeObserver,
    pub identity: String,
    /// Set once the terminal has seen session output (for resets).
    pub attached_once: bool,
    /// A reconnect status line / title is on the terminal (DESIGN §5.4).
    pub status_shown: bool,
}

/// How serving a link ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Leave with this exit code.
    Exit(u8),
    /// The link died; the session may still be there.
    LinkLost,
}

fn proxy_args(args: &ClientArgs, state: &State, resuming: bool) -> Vec<String> {
    match (&state.session, &args.target) {
        (Some(name), _) => {
            let mode = if resuming {
                Mode::Attach
            } else {
                Mode::AttachOrCreate
            };
            vec![
                "_proxy".into(),
                "--session".into(),
                name.clone(),
                "--mode".into(),
                crate::proxy::mode_arg(mode).into(),
            ]
        }
        (None, Target::New) => vec!["_proxy".into(), "--new".into()],
        (None, Target::Named(name)) => vec![
            "_proxy".into(),
            "--session".into(),
            name.clone(),
            "--mode".into(),
            "attach-or-create".into(),
        ],
    }
}

fn hello(args: &ClientArgs, state: &State, force: bool) -> Hello {
    let size = sys::get_winsize(STDIN).unwrap_or_default();
    let mode = match (&state.session, &args.target) {
        (Some(_), _) if state.instance.is_some() => Mode::Attach,
        (None, Target::New) => Mode::Create,
        _ => Mode::AttachOrCreate,
    };
    Hello {
        proto: proto::PROTO_VERSION,
        session: state
            .session
            .clone()
            .or_else(|| match &args.target {
                Target::Named(n) => Some(n.clone()),
                Target::New => None,
            })
            .unwrap_or_default(),
        mode,
        identity: state.identity.clone(),
        force,
        term: std::env::var("TERM").unwrap_or_default(),
        colorterm: std::env::var("COLORTERM").unwrap_or_default(),
        size,
        resume: state.instance.map(|instance| Resume {
            instance,
            offset: state.offset,
        }),
        command: args.command.clone(),
    }
}

// ---- running ---------------------------------------------------------------

/// Run the session until it ends or the user leaves; returns the exit code.
pub fn run(args: ClientArgs) -> u8 {
    if !sys::isatty(STDIN) {
        eprintln!("acs: stdin is not a terminal");
        return code::USAGE;
    }
    if let Err(e) = tty::install_emergency_restore() {
        eprintln!("acs: {e}");
        return code::ERROR;
    }
    let signals = match sys::signals::install(&[libc::SIGWINCH]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("acs: {e}");
            return code::ERROR;
        }
    };
    let _ = sys::signals::ignore(libc::SIGPIPE);
    let mut state = State {
        host: args.host_name().to_string(),
        session: match &args.target {
            Target::Named(n) => Some(n.clone()),
            Target::New => None,
        },
        instance: None,
        offset: 0,
        unacked: Unacked::new(0),
        observer: ModeObserver::new(),
        identity: identity(),
        attached_once: false,
        status_shown: false,
    };
    let mut raw: Option<RawMode> = None;
    let result = crate::reconnect::run(&args, &mut state, &mut raw, &signals);
    leave(&mut state, &mut raw);
    result
}

/// Put the local terminal back the way we found it.
pub fn leave(state: &mut State, raw: &mut Option<RawMode>) {
    let reset = state.observer.reset_sequence();
    if !reset.is_empty() {
        let _ = sys::write_all(STDOUT, &reset);
    }
    state.observer.clear();
    *raw = None;
}

/// Connect once: dial, handshake, and serve until the link ends.
/// `resuming` is true for a redial after a lost link.
pub fn connect_and_serve(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    resuming: bool,
) -> Outcome {
    let pargs = proxy_args(args, state, resuming);
    let pargs: Vec<&str> = pargs.iter().map(String::as_str).collect();
    let remote = ssh::remote_acs(crate::VERSION, &pargs);
    // Cooked mode while ssh may prompt (password, key touch).
    if let Some(r) = raw.as_mut() {
        let _ = r.suspend();
    }
    let (link, marker) = match dial(args, Call::Session, &remote) {
        Ok(x) => x,
        Err(e) => {
            if resuming {
                return Outcome::LinkLost;
            }
            note(&e.to_string());
            return Outcome::Exit(code::UNREACHABLE);
        }
    };
    let rest = match marker {
        Marker::Ready { proto: p, rest } => {
            if p != proto::PROTO_VERSION {
                note(&format!(
                    "remote acs speaks protocol {p}, this client {}",
                    proto::PROTO_VERSION
                ));
                link.close();
                return Outcome::Exit(code::ERROR);
            }
            rest
        }
        Marker::Need { os, arch } => {
            link.close();
            if !args.config.install_on_remote.value {
                note(&crate::install::not_installing(args, &os, &arch));
                return Outcome::Exit(code::INSTALL_FAILED);
            }
            return match crate::install::install(args, &os, &arch) {
                Ok(()) => connect_and_serve(args, state, raw, signals, resuming),
                Err(e) => {
                    note(&e);
                    Outcome::Exit(code::INSTALL_FAILED)
                }
            };
        }
    };
    serve(args, state, raw, signals, link, rest)
}

fn write_link(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<()> {
    while !buf.is_empty() {
        match sys::write(fd, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Send INPUT for bytes the user typed.
fn queue_input(state: &mut State, out: &mut Vec<u8>, bytes: &[u8]) {
    for chunk in bytes.chunks(proto::MAX_CHUNK) {
        let seq = state.unacked.push(chunk);
        Msg::Input {
            seq,
            bytes: chunk.to_vec(),
        }
        .encode(out);
    }
}

/// Serve one link: handshake, then relay until something ends it.
fn serve(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    link: Link,
    early: Vec<u8>,
) -> Outcome {
    let to = link.to.as_raw_fd();
    let from = link.from.as_raw_fd();
    let _ = sys::set_nonblocking(to, true);
    let _ = sys::set_nonblocking(from, true);
    let mut dec = Decoder::new();
    dec.push(&early);
    let mut out = Msg::Hello(hello(args, state, args.force)).to_bytes();
    let mut buf = vec![0u8; 64 * 1024];
    let mut detector = Detector::new(escape_config());
    let mut welcomed = false;
    let mut exiting = false;
    let mut liveness = crate::reconnect::Liveness::new();

    let lost = |link: Link| {
        link.close();
        Outcome::LinkLost
    };

    loop {
        // Decode everything available first.
        loop {
            let msg = match dec.next_msg() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    note(&format!("protocol error: {e}"));
                    return lost(link);
                }
            };
            liveness.heard();
            match msg {
                Msg::Welcome(w) if !welcomed => {
                    welcomed = true;
                    let first = state.instance.is_none();
                    let same = state.instance == Some(w.instance);
                    if !first && !same {
                        note("the session was restarted");
                    }
                    if w.created {
                        note(&format!("new session '{}' on {}", w.session, state.host));
                    }
                    state.session = Some(w.session.clone());
                    state.instance = Some(w.instance);
                    state.unacked.rebase(w.input_seq, same);
                    if raw.is_none() {
                        match RawMode::enter(STDIN) {
                            Ok(r) => *raw = Some(r),
                            Err(e) => {
                                note(&format!("cannot use the terminal: {e}"));
                                link.close();
                                return Outcome::Exit(code::ERROR);
                            }
                        }
                    } else if let Some(r) = raw.as_mut() {
                        let _ = r.resume();
                    }
                    match w.kind {
                        AttachKind::Resumed => {}
                        AttachKind::Fresh | AttachKind::Gap => {
                            if !w.created {
                                // dtach's attach: clear, the program redraws.
                                let _ = sys::write_all(STDOUT, b"\x1b[H\x1b[J");
                            }
                            state.observer.clear();
                        }
                    }
                    state.offset = w.offset;
                    state.attached_once = true;
                    // Resend what the master may not have (DESIGN §5.2).
                    if !state.unacked.is_empty() {
                        let (seq, bytes) = state.unacked.pending();
                        for (i, chunk) in bytes.chunks(proto::MAX_CHUNK).enumerate() {
                            Msg::Input {
                                seq: seq + (i * proto::MAX_CHUNK) as u64,
                                bytes: chunk.to_vec(),
                            }
                            .encode(&mut out);
                        }
                    }
                    crate::reconnect::on_welcome(state, w.kind, &mut out);
                }
                Msg::Busy { identity, since } if !welcomed => {
                    match crate::reconnect::ask_takeover(state, &identity, since) {
                        true => Msg::Hello(hello(args, state, true)).encode(&mut out),
                        false => {
                            link.close();
                            return Outcome::Exit(code::TAKEN_OVER);
                        }
                    }
                }
                Msg::Error { code: c, message } => {
                    note(&message);
                    link.close();
                    let exit = match c {
                        proto::err::NO_SESSION => {
                            if state.instance.is_some() {
                                note("the session has ended");
                            }
                            code::NO_SESSION
                        }
                        _ => code::ERROR,
                    };
                    return Outcome::Exit(exit);
                }
                Msg::Data { offset, bytes } if welcomed => {
                    // Skip anything we already wrote (never expected).
                    let skip = state.offset.saturating_sub(offset) as usize;
                    if skip < bytes.len() {
                        let new = &bytes[skip..];
                        if sys::write_all(STDOUT, new).is_err() {
                            link.close();
                            return Outcome::Exit(code::ERROR);
                        }
                        state.observer.observe(new);
                        state.offset = offset + bytes.len() as u64;
                    }
                }
                Msg::Ack { seq } => state.unacked.ack(seq),
                Msg::Pong(_) => {}
                Msg::Exit { status } => {
                    link.close();
                    return Outcome::Exit(sys::exit_code(status));
                }
                Msg::Takeover => {
                    link.close();
                    let name = state.session.clone().unwrap_or_default();
                    leave(state, raw);
                    note(&format!(
                        "another client took over {}/{name} — reattach with: acs {} {name}",
                        state.host, state.host
                    ));
                    return Outcome::Exit(code::TAKEN_OVER);
                }
                other => {
                    if args.verbose > 0 {
                        note(&format!("ignoring {other:?}"));
                    }
                }
            }
        }

        if let Err(_e) = write_link(to, &mut out) {
            return lost(link);
        }

        let now = sys::now_ms();
        let mut timeout: i64 = -1;
        let mut consider = |deadline: u64| {
            let d = deadline.saturating_sub(now) as i64;
            timeout = if timeout < 0 { d } else { timeout.min(d) };
        };
        if let Some(d) = detector.deadline() {
            consider(d);
        }
        if welcomed {
            consider(liveness.next_deadline_ms());
        }

        let mut fds = [
            sys::pollfd(from, libc::POLLIN),
            sys::pollfd(signals.as_raw_fd(), libc::POLLIN),
            sys::pollfd(
                STDIN,
                if welcomed && !exiting {
                    libc::POLLIN
                } else {
                    0
                },
            ),
            sys::pollfd(to, if out.is_empty() { 0 } else { libc::POLLOUT }),
        ];
        if sys::poll(&mut fds, timeout.min(i32::MAX as i64) as i32).is_err() {
            return lost(link);
        }

        if fds[1].revents != 0 {
            for sig in sys::signals::drain(signals.as_raw_fd()) {
                if sig == libc::SIGWINCH && welcomed {
                    if let Ok(s) = sys::get_winsize(STDIN) {
                        Msg::Resize(s).encode(&mut out);
                    }
                }
            }
        }

        // Keys: through the command detector.
        let mut action = None;
        if fds[2].revents != 0 {
            match sys::read(STDIN, &mut buf) {
                Ok(0) | Err(_) => {
                    // The terminal went away: leave the session running.
                    Msg::Detach.encode(&mut out);
                    let _ = write_link(to, &mut out);
                    link.close();
                    return Outcome::Exit(code::DETACHED);
                }
                Ok(n) => {
                    let o = detector.feed(&buf[..n], sys::now_ms());
                    queue_input(state, &mut out, &o.forward);
                    action = o.action;
                }
            }
        } else if detector.deadline().is_some_and(|d| sys::now_ms() >= d) {
            let o = detector.tick(sys::now_ms());
            queue_input(state, &mut out, &o.forward);
        }
        match action {
            Some(Action::Detach) => {
                Msg::Detach.encode(&mut out);
                let deadline = Instant::now() + Duration::from_millis(500);
                while !out.is_empty() && Instant::now() < deadline {
                    if write_link(to, &mut out).is_err() {
                        break;
                    }
                    let mut p = [sys::pollfd(to, libc::POLLOUT)];
                    let _ = sys::poll(&mut p, 50);
                }
                link.close();
                leave(state, raw);
                let name = state.session.clone().unwrap_or_default();
                note(&format!(
                    "detached from {}/{name} — reattach with: acs {} {name}",
                    state.host, state.host
                ));
                return Outcome::Exit(code::DETACHED);
            }
            Some(Action::Exit) => {
                Msg::Kill.encode(&mut out);
                exiting = true;
            }
            None => {}
        }

        if fds[0].revents != 0 {
            match sys::read(from, &mut buf) {
                Ok(0) => return lost(link),
                Ok(n) => dec.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return lost(link),
            }
        }

        if welcomed {
            match liveness.tick(&mut out) {
                crate::reconnect::Health::Ok => {}
                crate::reconnect::Health::Dead => return lost(link),
            }
        }
    }
}
