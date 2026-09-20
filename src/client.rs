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

/// Form feed: shells and most full-screen programs repaint on it.
const CTRL_L: u8 = 0x0c;

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

/// Entry point of the client: `args` after the program name, or with
/// `list` (`acs list`, DESIGN §7.3) after that word.
pub fn main(args: &[OsString], list: bool) -> ExitCode {
    let default = std::env::var("ACS_DEFAULT_SESSION").ok();
    let parsed = if list {
        cli::parse_list(args.iter().cloned())
    } else {
        cli::parse(args.iter().cloned(), default.as_deref())
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(code::USAGE);
        }
    };
    let (mut args, every_alias) = match parsed {
        Parsed::Help => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Parsed::Version => {
            crate::print_version();
            return ExitCode::SUCCESS;
        }
        Parsed::Run(a) => (*a, false),
        Parsed::ListAll(a) => (*a, true),
    };
    args.config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(code::USAGE);
        }
    };
    // Before connecting: a newer release found by an earlier check.
    crate::update_check::on_client_start(&args.config);
    // `acs list` in a terminal is the session menu; into a pipe, the table
    // (DESIGN §7.3).
    let menu = sys::isatty(STDIN) && sys::isatty(STDOUT);
    if every_alias {
        return match menu {
            true => ExitCode::from(crate::pick::every_host(&args)),
            false => crate::list::run_all(&args),
        };
    }
    let name = args.transport.destination.clone();
    if let Err(e) = resolve_alias(&mut args, &name) {
        eprintln!("acs: {e}");
        // Persisting (DESIGN §5.3): wait until one of its hosts answers.
        if args.list || !persist(&args) {
            return ExitCode::from(code::UNREACHABLE);
        }
        wait_for_host(&mut args, &name);
    }
    if args.list && !menu {
        return crate::list::run(&args);
    }
    // No session named: pick one on the host just resolved (DESIGN §4.4),
    // over the connection the session then uses.
    let always = args.list;
    loop {
        match crate::pick::choose(&mut args, always) {
            Ok(picked) => return ExitCode::from(run(args, picked)),
            // The host could not be reached for the menu: persisting, wait
            // until it answers a ping, then ask again.
            Err(code::UNREACHABLE) if !args.list && persist(&args) => {
                wait_for_host(&mut args, &name)
            }
            Err(c) => return ExitCode::from(c),
        }
    }
}

/// Whether a lost host is waited for (DESIGN §5.3): `--persist`, then
/// `ACS_PERSIST`, then the setting of the alias's entry in use, the
/// alias's, the global one.
pub(crate) fn persist(args: &ClientArgs) -> bool {
    if args.persist {
        return true;
    }
    let alias = args_alias(args);
    let entry = alias.zip(args.entry).and_then(|(a, i)| a.entries.get(i));
    env_switch(
        std::env::var("ACS_PERSIST").ok().as_deref(),
        args.config.persist_for(alias, entry).value,
    )
}

/// How often a lost host is pinged while it is waited for: the alias's
/// `reachability_interval`, else the global one.
pub(crate) fn reachability_interval(args: &ClientArgs) -> Duration {
    args.config
        .reachability_interval_for(args_alias(args))
        .value
}

/// The alias `args` go through, resolved or not yet: `[user@]<alias>` as
/// given.
fn args_alias(args: &ClientArgs) -> Option<&crate::config::Alias> {
    let name = args.alias.as_deref().unwrap_or(&args.transport.destination);
    args.config.alias(crate::alias::split_user(name).1)
}

/// Before any session: wait, pinging every `reachability_interval`, until
/// the host `name` stands for answers — an alias's hosts as resolving pings
/// them (and then points `args` at the one that answered), a plain host
/// itself. Ctrl-C gives up. A host that cannot be pinged (an alias whose
/// hosts all have `reachability_check: false`) is just waited for once.
pub(crate) fn wait_for_host(args: &mut ClientArgs, name: &str) {
    let every = reachability_interval(args);
    note(&format!(
        "waiting for {} to answer a ping, every {} (Ctrl-C gives up)",
        args.host_name(),
        crate::config::format_timeout(every)
    ));
    loop {
        std::thread::sleep(every);
        if host_answers(args, name).unwrap_or(true) {
            return;
        }
    }
}

/// Whether the host `name` stands for answers a ping now: an alias is
/// resolved again, pinging its hosts (and `args` then point at the one that
/// answered); a plain host is pinged. `None` when it cannot be pinged: an
/// alias whose hosts all have `reachability_check: false`.
pub(crate) fn host_answers(args: &mut ClientArgs, name: &str) -> Option<bool> {
    let alias_name = crate::alias::split_user(name).1;
    match args.config.alias(alias_name) {
        Some(a) if a.entries.iter().all(|e| !e.reachability_check) => None,
        Some(_) => Some(resolve_alias(args, name).is_ok()),
        None => {
            let host = crate::alias::split_user(&args.transport.destination).1;
            let timeout = args.config.reachability_timeout.value;
            Some(crate::alias::ping(host, timeout))
        }
    }
}

/// A session connection already open and past its marker: the pick's
/// (DESIGN §4.4), which the attach goes on over. `rest` is what arrived
/// after the pick's last frame.
pub struct Picked {
    pub link: Link,
    pub rest: Vec<u8>,
}

/// If `name` is an alias, or `user@<alias>`, point the transport at the host
/// it stands for now, with that entry's key (DESIGN §7.3); with `-v`, say
/// which entry was chosen and why. `name` is kept as given, so a redial
/// resolves it the same way.
pub fn resolve_alias(args: &mut ClientArgs, name: &str) -> Result<(), String> {
    let verbose = args.verbose > 0;
    let entry = crate::alias::resolve(
        name,
        &args.config,
        std::sync::Arc::new(crate::alias::ping),
        &crate::netmatch::system,
        &mut |m| {
            if verbose {
                note(&m)
            }
        },
    )?;
    if let Some(e) = entry {
        args.alias = Some(name.to_string());
        args.entry = args_alias(args).and_then(|a| {
            a.entries
                .iter()
                .position(|x| x.origin == e.origin && x.host == e.host)
        });
        args.transport.destination = e.destination();
        args.transport.identity_file = e
            .identity_file
            .as_ref()
            .map(|s| crate::config::expand_home(&s.value));
        if let (true, Some(key)) = (verbose, &e.identity_file) {
            let at = key
                .origin
                .as_ref()
                .map(|o| o.to_string())
                .unwrap_or_default();
            if args.transport.user_identity() {
                note(&format!(
                    "{name}: the key given on the command line replaces identity_file {} ({at})",
                    key.value
                ));
            } else {
                note(&format!("{name}: identity_file {} ({at})", key.value));
            }
        }
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

pub(crate) fn escape_config() -> keys::Config {
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

/// Whether arming command mode rings the bell (DESIGN §6.1).
pub(crate) fn command_bell(config: &crate::config::Config) -> bool {
    env_switch(
        std::env::var("ACS_COMMAND_BELL").ok().as_deref(),
        config.command_bell.value,
    )
}

/// Whether a reconnect sends Ctrl-L (DESIGN §5.2): `ACS_REDRAW_ON_RECONNECT`,
/// then the alias's `redraw_on_reconnect`, then the global one.
pub(crate) fn redraw_on_reconnect(args: &ClientArgs) -> bool {
    env_switch(
        std::env::var("ACS_REDRAW_ON_RECONNECT").ok().as_deref(),
        args.config
            .redraw_on_reconnect_for(args.alias.as_deref())
            .value,
    )
}

/// An `ACS_*` switch over the configuration: `0` is off, anything else
/// non-empty on.
fn env_switch(env: Option<&str>, config: bool) -> bool {
    match env {
        Some(v) if !v.is_empty() => v != "0",
        _ => config,
    }
}

/// Print a message on the terminal outside the session's byte stream.
/// Works in raw mode too (explicit `\r\n`).
pub fn note(msg: &str) {
    // Notes carry remote text (a session name, an identity, the message of
    // an ERROR frame), so nothing printed here may steer the terminal
    // (acs-w1z). The message is one line; the cap is generous enough for
    // the longest of them.
    let msg = crate::safe::display_max(msg, 4 * crate::safe::MAX_FIELD);
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

/// How long a connection may take to answer: to print its marker, and then
/// to WELCOME us (DESIGN §5.3). A first connection may wait on a password
/// or a key touch, so it gets longer than a redial, which must return to
/// the backoff wait (where the command keys work) if the host accepted the
/// connection and then went quiet. `ACS_DIAL_TIMEOUT_MS` sets both.
pub fn answer_timeout(redial: bool) -> Duration {
    let ms = std::env::var("ACS_DIAL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if redial { 30_000 } else { 120_000 });
    Duration::from_millis(ms)
}

/// Start the transport running `remote` and wait, at most `timeout`, for
/// the marker line.
pub fn dial(
    args: &ClientArgs,
    call: Call,
    remote: &str,
    timeout: Duration,
) -> io::Result<(Link, Marker)> {
    let deadline = Instant::now() + timeout;
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
    let mut child = sys::spawn(&mut cmd).map_err(|e| {
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
        let left = deadline.saturating_duration_since(Instant::now());
        let mut p = [sys::pollfd(from.as_raw_fd(), libc::POLLIN)];
        if left.is_zero() || sys::poll(&mut p, left.as_millis().min(i32::MAX as u128) as i32)? == 0
        {
            if Instant::now() < deadline {
                continue; // a signal cut the wait short
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "no answer from {} within {} s",
                    args.transport.destination,
                    timeout.as_secs_f32()
                ),
            ));
        }
        let n = sys::read(from.as_raw_fd(), &mut buf)?;
        if n == 0 {
            if let Some(noise) = scanner.noise_text().filter(|_| args.verbose > 0) {
                note(&format!("remote said: {noise}"));
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
    if let Some(noise) = scanner.noise_text().filter(|_| args.verbose > 0) {
        note(&format!("skipped remote login output: {noise:?}"));
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
    /// Ring the bell when command mode arms (DESIGN §6.1).
    pub command_bell: bool,
    /// A bell waits for the output to reach a boundary.
    pub bell_pending: bool,
    /// Send Ctrl-L after reconnecting to a session (DESIGN §5.2).
    pub redraw_on_reconnect: bool,
    /// Whether the input sent so far is inside a bracketed paste.
    pub paste: keys::PasteTracker,
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
        (None, Target::Named(name) | Target::Pick(name)) => vec![
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
                Target::Named(n) | Target::Pick(n) => Some(n.clone()),
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
/// `picked` is the first connection when the session menu opened it.
pub fn run(args: ClientArgs, picked: Option<Picked>) -> u8 {
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
            Target::Named(n) | Target::Pick(n) => Some(n.clone()),
            Target::New => None,
        },
        instance: None,
        offset: 0,
        unacked: Unacked::new(0),
        observer: ModeObserver::new(),
        identity: identity(),
        attached_once: false,
        status_shown: false,
        command_bell: command_bell(&args.config),
        bell_pending: false,
        redraw_on_reconnect: redraw_on_reconnect(&args),
        paste: keys::PasteTracker::default(),
    };
    let mut raw: Option<RawMode> = None;
    let result = crate::reconnect::run(&args, &mut state, &mut raw, &signals, picked);
    leave(&mut state, &mut raw);
    result
}

/// Put the local terminal back the way we found it.
pub fn leave(state: &mut State, raw: &mut Option<RawMode>) {
    crate::reconnect::clear_status(state);
    let reset = state.observer.reset_sequence();
    if !reset.is_empty() {
        let _ = sys::write_all(STDOUT, &reset);
    }
    state.observer.clear();
    *raw = None;
}

/// Connect once: dial, handshake, and serve until the link ends.
/// `resuming` is true for a redial after a lost link. `picked`, a
/// connection the session menu opened, is used instead of dialing.
pub fn connect_and_serve(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    resuming: bool,
    picked: Option<Picked>,
) -> Outcome {
    let timeout = answer_timeout(resuming);
    if let Some(p) = picked {
        return serve(args, state, raw, signals, p.link, p.rest, timeout);
    }
    let pargs = proxy_args(args, state, resuming);
    let pargs: Vec<&str> = pargs.iter().map(String::as_str).collect();
    let remote = ssh::remote_acs(crate::VERSION, &pargs);
    // Cooked mode while ssh may prompt (password, key touch).
    if let Some(r) = raw.as_mut() {
        let _ = r.suspend();
    }
    let (link, marker) = match dial(args, Call::Session, &remote, timeout) {
        Ok(x) => x,
        Err(e) => {
            if resuming {
                return Outcome::LinkLost;
            }
            note(&e.to_string());
            // Persisting, the first connection's failure is waited out
            // like a later one's (DESIGN §5.3).
            return match persist(args) {
                true => Outcome::LinkLost,
                false => Outcome::Exit(code::UNREACHABLE),
            };
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
                Ok(()) => connect_and_serve(args, state, raw, signals, resuming, None),
                Err(e) => {
                    note(&e);
                    Outcome::Exit(code::INSTALL_FAILED)
                }
            };
        }
    };
    serve(args, state, raw, signals, link, rest, timeout)
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

/// After feeding or ticking the command detector: ring the bell when
/// command mode has just armed, and drop a bell still waiting once it is
/// over (DESIGN §6.1).
pub fn follow_detector(state: &mut State, was_armed: bool, detector: &Detector) {
    if !detector.armed() {
        state.bell_pending = false;
    } else if !was_armed && state.command_bell {
        // Only between sequences: a BEL inside the program's OSC would end
        // it early. Otherwise it goes with the output that gets there.
        if state.observer.at_boundary() {
            let _ = sys::write_all(STDOUT, b"\x07");
        } else {
            state.bell_pending = true;
        }
    }
}

/// Write session output to the terminal, with a waiting bell at the first
/// boundary in it.
fn write_output(state: &mut State, bytes: &[u8]) -> io::Result<()> {
    if !state.bell_pending {
        sys::write_all(STDOUT, bytes)?;
        state.observer.observe(bytes);
        return Ok(());
    }
    let Some(n) = state.observer.observe_to_boundary(bytes) else {
        return sys::write_all(STDOUT, bytes);
    };
    state.bell_pending = false;
    let mut buf = Vec::with_capacity(bytes.len() + 1);
    buf.extend_from_slice(&bytes[..n]);
    buf.push(0x07);
    buf.extend_from_slice(&bytes[n..]);
    sys::write_all(STDOUT, &buf)?;
    state.observer.observe(&bytes[n..]);
    Ok(())
}

/// Send INPUT for bytes for the program: what the user typed, or the
/// Ctrl-L after a reconnect.
fn queue_input(state: &mut State, out: &mut Vec<u8>, bytes: &[u8]) {
    state.paste.feed(bytes);
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
    handshake: Duration,
) -> Outcome {
    // The host has until then to WELCOME us (acs-znr): liveness only starts
    // with the WELCOME.
    let mut handshake_until = sys::now_ms() + handshake.as_millis() as u64;
    let to = link.to.as_raw_fd();
    let from = link.from.as_raw_fd();
    let _ = sys::set_nonblocking(to, true);
    let _ = sys::set_nonblocking(from, true);
    let mut dec = Decoder::new();
    dec.push(&early);
    let mut out = Msg::Hello(hello(args, state, args.force)).to_bytes();
    let mut buf = vec![0u8; 64 * 1024];
    let mut detector = Detector::new(escape_config());
    // A bell waiting from an earlier link's command mode is moot.
    state.bell_pending = false;
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
                    if !same {
                        // Another program's input: no paste of ours is open.
                        state.paste = keys::PasteTracker::default();
                    }
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
                        // Keys typed during the redial were for a link that
                        // was down: dropped, as while waiting (DESIGN §5.2).
                        let _ = r.resume_discarding();
                    }
                    match w.kind {
                        AttachKind::Resumed => {}
                        AttachKind::Fresh => {
                            // Another program: undo the modes the last one
                            // left on before forgetting them, or leave()
                            // could not (acs-xk4).
                            let reset = state.observer.reset_sequence();
                            if !reset.is_empty() {
                                let _ = sys::write_all(STDOUT, &reset);
                            }
                            if !w.created {
                                // dtach's attach: clear, the program redraws.
                                let _ = sys::write_all(STDOUT, b"\x1b[H\x1b[J");
                            }
                            state.observer.clear();
                        }
                        AttachKind::Gap => {
                            // The same program, still in its modes: keep
                            // them for leave(), but not a half-seen sequence.
                            let _ = sys::write_all(STDOUT, b"\x1b[H\x1b[J");
                            state.observer.resync();
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
                    // Then Ctrl-L, so the program repaints (DESIGN §5.2):
                    // after what was typed before the drop, before anything
                    // typed now, and as input, so a later resume resends it
                    // like any key and the master writes it once. Not for a
                    // program just started, which has nothing to repaint,
                    // nor into a paste the program has not seen the end of.
                    if !w.created && state.redraw_on_reconnect && !state.paste.open() {
                        queue_input(state, &mut out, &[CTRL_L]);
                    }
                    crate::reconnect::on_welcome(state, w.kind, &mut out);
                }
                Msg::Busy { identity, since } if !welcomed => {
                    match crate::reconnect::ask_takeover(state, &identity, since) {
                        true => {
                            // The question took the user's time, not the host's.
                            handshake_until = sys::now_ms() + handshake.as_millis() as u64;
                            Msg::Hello(hello(args, state, true)).encode(&mut out)
                        }
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
                        if write_output(state, &bytes[skip..]).is_err() {
                            link.close();
                            return Outcome::Exit(code::ERROR);
                        }
                        state.offset = offset + bytes.len() as u64;
                    }
                }
                Msg::Ack { seq } => state.unacked.ack(seq),
                Msg::Pong(_) => {}
                // The master checks on us too (DESIGN §5.3, acs-ode).
                Msg::Ping(n) => Msg::Pong(n).encode(&mut out),
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
        } else {
            consider(handshake_until);
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
                    let was = detector.armed();
                    let o = detector.feed(&buf[..n], sys::now_ms());
                    follow_detector(state, was, &detector);
                    queue_input(state, &mut out, &o.forward);
                    action = o.action;
                }
            }
        } else if detector.deadline().is_some_and(|d| sys::now_ms() >= d) {
            let was = detector.armed();
            let o = detector.tick(sys::now_ms());
            follow_detector(state, was, &detector);
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
                Ok(n) => {
                    dec.push(&buf[..n]);
                    // Bytes from the host are proof it is there. Their
                    // frames are decoded at the top of the next iteration,
                    // after the tick below, so without this a terminal that
                    // stopped reading for longer than the dead interval —
                    // write_output blocks meanwhile — would look like a lost
                    // link, with PONGs sitting in the pipe (acs-7k7).
                    liveness.heard();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return lost(link),
            }
        }

        if welcomed {
            match liveness.tick(&mut out) {
                crate::reconnect::Health::Ok => {}
                crate::reconnect::Health::Dead => return lost(link),
            }
        } else if sys::now_ms() >= handshake_until {
            // Accepted, then silent: a redial goes back to its backoff; a
            // first connection gives up.
            if state.instance.is_some() {
                return lost(link);
            }
            link.close();
            note(&format!(
                "no answer from {} within {} s",
                args.transport.destination,
                handshake.as_secs_f32()
            ));
            return Outcome::Exit(code::UNREACHABLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_decides_a_switch_over_the_configuration() {
        assert!(env_switch(None, true));
        assert!(!env_switch(None, false));
        assert!(env_switch(Some(""), true), "empty is unset");
        assert!(!env_switch(Some("0"), true));
        assert!(env_switch(Some("1"), false));
    }

    /// acs-txt: `persist` is the entry in use's, else the alias's, else the
    /// global setting; `--persist` beats them all. (ACS_PERSIST is
    /// `env_switch`, above: the process environment is not set here, since
    /// tests run in parallel.)
    #[test]
    fn persist_follows_entry_alias_global_and_the_flag_wins() {
        let dir = crate::testutil::TempDir::new();
        let f = dir.path().join("c.yaml");
        std::fs::write(
            &f,
            "\
persist: true
reachability_interval: 7s
aliases:
  off:
    persist: false
    reachability_interval: 300ms
    hosts:
      - host: a
        persist: true
      - host: b
  plain: [{host: c}]
",
        )
        .unwrap();
        let config = crate::config::Config::load_files(&[f]).unwrap();
        let args = |dest: &str, flag: bool, entry: Option<usize>| {
            let mut a = match cli::parse([dest].iter().map(OsString::from), None).unwrap() {
                Parsed::Run(a) => *a,
                other => panic!("{other:?}"),
            };
            a.config = config.clone();
            a.persist = flag;
            a.entry = entry;
            a
        };
        // The entry in use, then the alias, then the global setting.
        assert!(persist(&args("off", false, Some(0))));
        assert!(!persist(&args("off", false, Some(1))));
        assert!(!persist(&args("off", false, None)));
        assert!(persist(&args("plain", false, Some(0))));
        assert!(persist(&args("elsewhere", false, None)));
        // The flag over everything.
        assert!(persist(&args("off", true, Some(1))));
        // The interval: the alias's, else the global one.
        assert_eq!(
            reachability_interval(&args("off", false, None)),
            Duration::from_millis(300)
        );
        assert_eq!(
            reachability_interval(&args("me@plain", false, None)),
            Duration::from_secs(7)
        );
    }
}
