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
use crate::timing::Timing;
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
    // The master acs keeps for this host, if it may have one: decided once,
    // before anything dials, so every call this client makes agrees about
    // it (acs-9n3). Never for `acs list` over every alias, which asks a
    // dozen hosts at once and would leave a master on each.
    let decided = crate::mux::configure(&mut args.transport);
    if args.verbose > 0 {
        note(&decided);
    }
    let name = args.transport.destination.clone();
    let mut timing = Timing::start(args.verbose > 0, "first connection");
    if let Err(e) = resolve_alias(&mut args, &name) {
        eprintln!("acs: {e}");
        // Persisting (DESIGN §5.3): wait until one of its hosts answers.
        if args.list || !persist(&args) {
            return ExitCode::from(code::UNREACHABLE);
        }
        wait_for_host(&mut args, &name);
    }
    if args.alias.is_some() {
        timing.mark("alias resolved");
    }
    if args.list && !menu {
        return crate::list::run(&args);
    }
    // No session named: pick one on the host just resolved (DESIGN §4.4),
    // over the connection the session then uses.
    let always = args.list;
    loop {
        match crate::pick::choose(&mut args, always, &mut timing) {
            Ok(picked) => return ExitCode::from(run(args, picked, timing)),
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
///
/// Bounded and control-free before it leaves (acs-ovq): this ends up in
/// another person's terminal, in `acs list` and in the session's trail.
/// The master holds it to the same rule on the way in — this side is a
/// courtesy, not the check.
pub fn identity() -> String {
    let set = std::env::var("ACS_IDENTITY")
        .ok()
        .map(|s| crate::safe::display_max(s.trim(), 128))
        .filter(|s| !s.is_empty());
    set.unwrap_or_else(|| {
        let user = sys::user_name(sys::getuid()).unwrap_or_else(|| "user".into());
        crate::safe::display_max(&format!("{user}@{}", sys::hostname()), 128)
    })
}

/// The command key and its window (DESIGN §6.1). `ACS_ESCAPE_TIMEOUT_MS` is
/// the one window setting: it sets the gap allowed between the two presses,
/// and `keys::Config` reads the window to choose the command key off it too.
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
    note_max(msg, 4 * crate::safe::MAX_FIELD)
}

/// [`note`] with a cap of its own.
///
/// For a message built entirely from what acs is about to run rather than
/// from anything a remote said: the `-v` line naming the ssh command line
/// carries the whole prelude, which is over a kilobyte since acs-gov, and
/// [`note`]'s cap cut it off in the middle — taking with it the acs
/// arguments at the end, the one part of that line that differs from one
/// dial to the next. Still sanitised, since a destination or a key path
/// comes from a configuration file.
pub fn note_max(msg: &str, max: usize) {
    let msg = crate::safe::display_max(msg, max);
    notes::raise(format!("acs: {msg}\r\n"));
}

/// Where a note may be written (acs-z22).
///
/// Notes go to fd 2 and the session's bytes to fd 1, and under `-v` both
/// land on the same terminal. Nothing in the stream separates them, so a
/// note raised while the program is halfway through an escape sequence, an
/// OSC string or a UTF-8 character is written *into* it: the sequence is
/// split, or the note's own text is swallowed as the body of a title. That
/// is at its worst under `-v`, which is what someone reaches for when
/// something is already wrong.
///
/// The rule is the bell's (DESIGN §6.1): acs may put a byte of its own only
/// at a **boundary** of the output, which the mode observer (§6.4) already
/// knows. So while [`serve`] is relaying — and only then — a note raised
/// with the stream mid-sequence waits, and is written at the first point
/// where [`ModeObserver::at_boundary`] holds again, which is inside the
/// frame that gets there rather than after it.
///
/// What that must not become is a note that is late, lost or out of order:
///
/// - **Nothing outside the frame loop waits.** The gate is open only for
///   the length of a [`serve`] call, and even inside it a note raised with
///   the stream at a boundary — which is where it is between frames, and
///   always before the first one — is written at once. The dial, the
///   offline wait and alias resolution have no frame in flight and are
///   never delayed: a note about a dial that is still hanging is worth
///   nothing after the dial has finished.
/// - **Order is kept.** Held notes queue oldest-first and are written in
///   that order, ahead of anything raised after the boundary.
/// - **The boundary is the stream's, not the frame's.** A frame is not an
///   atom of the program's output — a sequence is split across two of them
///   precisely because the host framed it that way — so the writer looks
///   for the first boundary *inside* the next frame and puts the notes
///   there, as it splices the bell. Waiting for a frame that happens to
///   end at a boundary would leave a note behind steady output for as long
///   as the output lasts.
/// - **A quiet session cannot swallow one.** A program that stops
///   mid-sequence, or a link that dies there, would otherwise hold a note
///   for ever. The wait is bounded by `ACS_NOTE_HOLD_MS` (500 ms), after
///   which the note is written where it stands, and leaving [`serve`]
///   flushes what is left whatever the stream was doing.
mod notes {
    use std::cell::RefCell;

    use crate::sys;

    /// How long a note waits for the stream to reach a boundary before it
    /// is written anyway (`ACS_NOTE_HOLD_MS`). Long enough for the rest of
    /// a sequence a frame boundary cut in half to arrive over a slow link;
    /// short enough that a diagnostic is never lost for long.
    fn hold_ms() -> u64 {
        crate::reconnect::env_ms("ACS_NOTE_HOLD_MS", 500)
    }

    thread_local! {
        /// Open while the frame loop is relaying; `None` everywhere else,
        /// where every note goes straight out.
        static GATE: RefCell<Option<Gate>> = const { RefCell::new(None) };
        /// Where notes go in this crate's own tests, in place of fd 2.
        #[cfg(test)]
        static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    #[derive(Default)]
    struct Gate {
        /// The last frame written left the terminal inside a sequence or a
        /// character: a note written now would land in the middle of it.
        mid_sequence: bool,
        /// Notes waiting for the boundary, oldest first.
        held: Vec<String>,
        /// When the oldest of them started waiting.
        since: u64,
    }

    /// The frame loop is relaying: notes raised mid-sequence wait. Dropping
    /// it writes whatever is still held and hands the terminal back.
    pub struct Relaying(());

    impl Relaying {
        pub fn open() -> Relaying {
            GATE.with(|g| *g.borrow_mut() = Some(Gate::default()));
            Relaying(())
        }
    }

    impl Drop for Relaying {
        fn drop(&mut self) {
            flush();
            GATE.with(|g| *g.borrow_mut() = None);
        }
    }

    /// Print `line`, or hold it if the program's stream is mid-sequence.
    pub fn raise(line: String) {
        let straight_out = GATE.with(|g| {
            let mut g = g.borrow_mut();
            match g.as_mut() {
                Some(gate) if gate.mid_sequence => {
                    if gate.held.is_empty() {
                        gate.since = sys::now_ms();
                    }
                    gate.held.push(line);
                    None
                }
                _ => Some(line),
            }
        });
        if let Some(line) = straight_out {
            write(&line);
        }
    }

    /// Whether anything is waiting for a boundary, so the writer knows to
    /// look for one inside the frame it is about to write.
    pub fn waiting() -> bool {
        GATE.with(|g| {
            g.borrow()
                .as_ref()
                .is_some_and(|gate| !gate.held.is_empty())
        })
    }

    /// Where the program's stream stands after a frame was written to the
    /// terminal. A boundary releases what was held for it.
    pub fn at_boundary(at: bool) {
        let release = GATE.with(|g| {
            let mut g = g.borrow_mut();
            match g.as_mut() {
                Some(gate) => {
                    gate.mid_sequence = !at;
                    at && !gate.held.is_empty()
                }
                None => false,
            }
        });
        if release {
            flush();
        }
    }

    /// When the oldest held note is written whatever the stream is doing.
    pub fn deadline() -> Option<u64> {
        GATE.with(|g| {
            g.borrow()
                .as_ref()
                .filter(|gate| !gate.held.is_empty())
                .map(|gate| gate.since + hold_ms())
        })
    }

    /// Write the held notes if they have waited long enough: the program
    /// stopped mid-sequence, or the bytes that would finish it are not
    /// coming.
    pub fn flush_due(now: u64) {
        if deadline().is_some_and(|d| now >= d) {
            flush();
        }
    }

    fn flush() {
        let held = GATE.with(|g| {
            let mut g = g.borrow_mut();
            g.as_mut()
                .map(|gate| std::mem::take(&mut gate.held))
                .unwrap_or_default()
        });
        for line in &held {
            write(line);
        }
    }

    fn write(line: &str) {
        #[cfg(test)]
        {
            let captured = SINK.with(|s| match s.borrow_mut().as_mut() {
                Some(lines) => {
                    lines.push(line.to_string());
                    true
                }
                None => false,
            });
            if captured {
                return;
            }
        }
        let _ = sys::write_all(2, line.as_bytes());
    }

    /// Collect this thread's notes instead of printing them.
    #[cfg(test)]
    pub fn capture() {
        SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
    }

    /// The notes written since the last call.
    #[cfg(test)]
    pub fn written() -> Vec<String> {
        SINK.with(|s| {
            s.borrow_mut()
                .as_mut()
                .map(std::mem::take)
                .expect("capture() first")
        })
    }
}

// ---- links -----------------------------------------------------------------

/// One connection to the remote: the transport child and its pipes.
pub struct Link {
    child: Child,
    to: OwnedFd,
    from: OwnedFd,
    /// Frames for the host that have not been written yet: the HELLO
    /// [`dial`] could not fit into the transport's stdin pipe in one go,
    /// or the one a link the session menu opened still owes (DESIGN §5.3).
    /// [`serve`] takes them as the first thing it writes, so nothing can
    /// overtake them.
    pending: Vec<u8>,
    /// Whether this connection is a channel on the master acs keeps for
    /// the host (`mux.rs`). Only such a link can take the master down with
    /// it when it dies, and only for the endings [`LinkEnd`] blames on the
    /// connection rather than on the channel.
    pub muxed: bool,
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
/// the marker line; `timing` is told when ssh is spawned and the marker
/// seen.
///
/// `first` goes into the transport's stdin the moment it is spawned,
/// before the marker is awaited (DESIGN §5.3, acs-trw): the remote side
/// reads nothing until it has printed the marker, so a HELLO left waiting
/// in the pipe saves a round trip on every dial and redial. Whatever does
/// not fit stays in [`Link::pending`] for [`serve`] to write.
///
/// **A master must answer fast or not at all** (acs-9n3). Where this dial
/// would join a master acs already has up, it is given
/// `ACS_CONTROL_FALLBACK_MS` rather than `timeout`: joining costs one
/// round trip, so anything slower is a master whose connection has died
/// without noticing. That one is killed and the dial made again on a
/// connection of its own, with the whole `timeout` — where a password or
/// a key touch may legitimately take a minute.
pub fn dial(
    args: &ClientArgs,
    call: Call,
    remote: &str,
    timeout: Duration,
    timing: &mut Timing,
    first: Vec<u8>,
) -> io::Result<(Link, Marker)> {
    if crate::mux::joinable(&args.transport, call) {
        timing.mark("shared ssh master checked");
        let window = timeout.min(crate::mux::fallback_window());
        match dial_once(args, call, remote, window, timing, first.clone()) {
            Ok(x) => return Ok(x),
            // A master that is up but cannot carry a channel is worse than
            // none: take it down, so the dial below and the next client
            // both make their own connection.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::UnexpectedEof
                ) =>
            {
                note(&format!(
                    "the shared ssh master for {} did not answer within {} s — dialing its own connection",
                    args.transport.destination,
                    window.as_secs_f32()
                ));
                crate::mux::stop(&args.transport);
            }
            Err(e) => return Err(e),
        }
    }
    dial_once(args, call, remote, timeout, timing, first)
}

fn dial_once(
    args: &ClientArgs,
    call: Call,
    remote: &str,
    timeout: Duration,
    timing: &mut Timing,
    first: Vec<u8>,
) -> io::Result<(Link, Marker)> {
    let deadline = Instant::now() + timeout;
    let mut cmd = args.transport.command(call, remote);
    if args.verbose > 0 {
        note_max(
            &format!(
                "running {}",
                ssh::display_argv(&args.transport.argv(call, remote))
            ),
            16 * crate::safe::MAX_FIELD,
        );
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
    timing.mark("ssh spawned");
    let to: OwnedFd = child.stdin.take().unwrap().into();
    let from: OwnedFd = child.stdout.take().unwrap().into();
    // The first frames, before anything is read back. Non-blocking, so a
    // transport that has not read a byte yet — it has not even connected —
    // cannot stall the dial: what the pipe takes goes now, the rest goes
    // with the first write of the session. A write that fails says nothing
    // the read loop below will not say better.
    let mut pending = first;
    if !pending.is_empty() {
        let _ = sys::set_nonblocking(to.as_raw_fd(), true);
        let _ = write_link(to.as_raw_fd(), &mut pending);
        let _ = sys::set_nonblocking(to.as_raw_fd(), false);
    }
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
    timing.mark(match marker {
        Marker::Ready { .. } => "ACS-READY seen",
        Marker::Need { .. } => "ACS-NEED seen",
    });
    if let Some(noise) = scanner.noise_text().filter(|_| args.verbose > 0) {
        note(&format!("skipped remote login output: {noise:?}"));
    }
    Ok((
        Link {
            child,
            to,
            from,
            pending,
            muxed: args.transport.multiplexes(call),
        },
        marker,
    ))
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
    /// The command-key detector (DESIGN §6.1). It belongs to the local
    /// terminal, not to a link, so there is one of it for as long as the
    /// client runs: a press held when the link dies, when a backoff
    /// expires into a redial attempt, or when the session comes back is
    /// still the first half of the double tap on the other side. The
    /// escape window is the only thing that ends a half-finished gesture
    /// (acs-e80).
    pub detector: Detector,
    /// Take the session from whoever holds it on the **next** attach, and
    /// then stop (acs-y5r). Agreement to a takeover is about the person
    /// who was attached when it was given, so it is spent by the attach it
    /// was given for rather than riding along on every later redial.
    pub force_next: bool,
    /// The transport of the link now in hand, when that link is a channel
    /// on acs's shared ssh master (acs-9n3) — the redial after it may end
    /// that master ([`LinkEnd::failed_the_connection`]).
    ///
    /// The transport, not a flag: an alias is resolved again **before**
    /// every redial (DESIGN §7.3), so by the time the redial runs
    /// `ssh -O exit` the destination may already be another host — whose
    /// master is somebody else's live connection, and whose socket is not
    /// the one that just died.
    pub master: Option<ssh::Transport>,
}

/// Why a link ended, so far as this side can tell (acs-n1m). It decides
/// one thing: whether the connection the link ran on is still worth
/// anything, and so whether the redial ends the master that link was a
/// channel on (DESIGN §7.1).
///
/// Only what the serve loop actually observed is in here. The far end
/// saying *why* it is finished — the session ended, somebody took it over,
/// the host refused us — is not a lost link at all: those are
/// [`Outcome::Exit`], and an `acs` that leaves on one never touches the
/// master, so a sibling on it keeps its connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEnd {
    /// The transport is gone: EOF on its pipes, a write to it that failed,
    /// or the poll watching them failing. ssh exits when its connection
    /// does — that is what `ServerAliveInterval=0` (DESIGN §3) leaves it
    /// to the TCP stack to notice — so this is the connection having
    /// failed until something says otherwise.
    Transport,
    /// Nothing came back in time: `ACS_DEAD_MS` of silence, or
    /// `ACS_NETCHECK_MS` after this machine's network moved (acs-ft1), or
    /// a handshake that was accepted and then went quiet. The transport is
    /// still running and the host is not answering.
    Silent,
    /// Bytes arrived on the link and ended it — a frame this client could
    /// not decode. The connection carried those bytes a moment ago, so it
    /// is up; what broke is the conversation on this one channel.
    Channel,
}

impl LinkEnd {
    /// Whether the evidence says the **connection** failed rather than
    /// only this channel on it — and so whether the redial should take the
    /// master the lost link ran on down with it (DESIGN §7.1, acs-n1m).
    ///
    /// [`LinkEnd::Silent`] is on the blunt side on purpose: from here a
    /// master whose TCP died without noticing looks exactly like a network
    /// that stopped answering, and of the two mistakes, keeping a wedged
    /// master is the one that strands somebody.
    pub fn failed_the_connection(self) -> bool {
        match self {
            LinkEnd::Transport | LinkEnd::Silent => true,
            LinkEnd::Channel => false,
        }
    }
}

/// How serving a link ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Leave with this exit code.
    Exit(u8),
    /// The link died, and how; the session may still be there.
    LinkLost(LinkEnd),
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
/// `picked` is the first connection when the session menu opened it;
/// `timing`, the first connection's clock.
pub fn run(args: ClientArgs, picked: Option<Picked>, timing: Timing) -> u8 {
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
        detector: Detector::new(escape_config()),
        force_next: args.force,
        master: picked
            .as_ref()
            .filter(|p| p.link.muxed)
            .map(|_| args.transport.clone()),
    };
    let mut raw: Option<RawMode> = None;
    let result = crate::reconnect::run(&args, &mut state, &mut raw, &signals, picked, timing);
    leave(&mut state, &mut raw);
    result
}

/// Put the local terminal back the way we found it.
///
/// A sequence the program left unfinished is **ended** here and not handed
/// back (acs-p4u, unlike [`write_over_stream`]): acs is going, nothing more
/// of that stream will ever arrive, and a terminal left inside a CSI would
/// eat the first bytes of whatever runs next — the shell's own prompt.
pub fn leave(state: &mut State, raw: &mut Option<RawMode>) {
    crate::reconnect::clear_status(state);
    let mut out = state.observer.interrupt_sequence().to_vec();
    out.extend_from_slice(&state.observer.reset_sequence());
    if !out.is_empty() {
        let _ = sys::write_all(STDOUT, &out);
    }
    state.observer.clear();
    *raw = None;
}

/// Write bytes of acs's own — the status line (DESIGN §5.4) — over a stream
/// that carries on afterwards (acs-p4u).
///
/// They cannot wait for a boundary the way a bell or an `acs:` note does
/// (§7): the status line explains a terminal that has just stopped
/// responding, and the boundary it would wait for may never come. So the
/// program's unfinished sequence is ended before them and re-opened after,
/// leaving the stream where acs found it — for the CSI or the character
/// that can be re-opened, which is the common case; where it cannot be
/// ([`ModeObserver::reopen_sequence`]), ending it is still better than
/// writing into it.
pub fn write_over_stream(state: &State, bytes: &[u8]) {
    let mut out = state.observer.interrupt_sequence().to_vec();
    out.extend_from_slice(bytes);
    out.extend_from_slice(&state.observer.reopen_sequence().unwrap_or_default());
    let _ = sys::write_all(STDOUT, &out);
}

/// Connect once: dial, handshake, and serve until the link ends.
/// `resuming` is true for a redial after a lost link, and `ended` is how
/// the link before this one ended — `None` before there was one (acs-n1m).
/// `picked`, a connection the session menu opened, is used instead of
/// dialing. `timing` is this connection's clock. `netwatch` is the client's
/// network watcher, polled here too so a change is noticed while the link
/// is up and not only while it is down (acs-ft1).
#[allow(clippy::too_many_arguments)]
pub fn connect_and_serve(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    resuming: bool,
    ended: Option<LinkEnd>,
    picked: Option<Picked>,
    timing: &mut Timing,
    netwatch: Option<&crate::netwatch::NetWatch>,
) -> Outcome {
    let timeout = answer_timeout(resuming);
    // Built before the dial so it can go out with it (acs-trw); the size
    // it carries is the terminal's as of now, and `serve` follows a
    // SIGWINCH that lands while the handshake is in flight.
    let greeting = hello(args, state, state.force_next);
    let size = greeting.size;
    let greeting = Msg::Hello(greeting).to_bytes();
    if let Some(p) = picked {
        // The menu's connection is past its marker and owes its HELLO:
        // the proxy read the list's frames from it first (DESIGN §4.4).
        let mut link = p.link;
        link.pending = greeting;
        state.master = link.muxed.then(|| args.transport.clone());
        return serve(
            args, state, raw, signals, link, p.rest, timeout, timing, size, netwatch,
        );
    }
    // The link that just died was a channel on acs's master. Whether the
    // master goes with it depends on what ended the link (acs-n1m): a
    // transport that is gone, or a host that stopped answering, is the
    // master's own connection having failed, and leaving it up would
    // answer the next client with a dead path (acs-9n3). A channel that
    // broke while the connection kept carrying bytes is not — ending the
    // master there drops every *other* acs session on it, each losing what
    // its user had typed (DESIGN §5.2), for a connection that was fine.
    //
    // The transport the link *ran on*, which is not `args.transport` any
    // more if the alias resolved to another host a moment ago: that host's
    // master is a live connection of somebody else's.
    if let Some(old) = state.master.take().filter(|_| resuming) {
        // No reason means no evidence, and this is the direction to be
        // wrong in: a sibling redials, a stranded user waits.
        if ended.map_or(true, LinkEnd::failed_the_connection) {
            if args.verbose > 0 {
                note(&format!(
                    "ending the shared ssh master the lost link ran on ({})",
                    old.destination
                ));
            }
            crate::mux::stop(&old);
        } else if args.verbose > 0 {
            note(&format!(
                "keeping the shared ssh master ({}): the channel ended, not the connection",
                old.destination
            ));
        }
    }
    // A redial never multiplexes (DESIGN §3): its whole job is to get a
    // connection of its own after one was lost.
    let call = match resuming {
        true => Call::Redial,
        false => Call::Session,
    };
    let pargs = proxy_args(args, state, resuming);
    let pargs: Vec<&str> = pargs.iter().map(String::as_str).collect();
    let remote = ssh::remote_acs(crate::VERSION, &pargs);
    // Cooked mode while ssh may prompt (password, key touch).
    if let Some(r) = raw.as_mut() {
        let _ = r.suspend();
    }
    let (link, marker) = match dial(args, call, &remote, timeout, timing, greeting) {
        Ok(x) => x,
        Err(e) => {
            // The dial itself did not come up: nothing reached the host,
            // so this is the connection, not a channel on one.
            if resuming {
                return Outcome::LinkLost(LinkEnd::Transport);
            }
            note(&e.to_string());
            // Persisting, the first connection's failure is waited out
            // like a later one's (DESIGN §5.3).
            return match persist(args) {
                true => Outcome::LinkLost(LinkEnd::Transport),
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
                Ok(()) => {
                    timing.mark("acs installed");
                    connect_and_serve(
                        args, state, raw, signals, resuming, ended, None, timing, netwatch,
                    )
                }
                Err(e) => {
                    note(&e);
                    Outcome::Exit(code::INSTALL_FAILED)
                }
            };
        }
    };
    state.master = link.muxed.then(|| args.transport.clone());
    serve(
        args, state, raw, signals, link, rest, timeout, timing, size, netwatch,
    )
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
pub fn follow_detector(state: &mut State, was_armed: bool) {
    if !state.detector.armed() {
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

/// Write session output to the terminal, with a waiting bell — and a note
/// waiting for the same thing (acs-z22) — at the first boundary in it.
fn write_output(state: &mut State, bytes: &[u8]) -> io::Result<()> {
    let r = write_frame(state, bytes);
    // Where the frame left the stream is where the next note may go, until
    // a frame moves it on.
    notes::at_boundary(state.observer.at_boundary());
    r
}

fn write_frame(state: &mut State, bytes: &[u8]) -> io::Result<()> {
    if !state.bell_pending && !notes::waiting() {
        sys::write_all(STDOUT, bytes)?;
        state.observer.observe(bytes);
        return Ok(());
    }
    // Nowhere in this frame may acs put anything of its own: it is all one
    // unfinished sequence, and what is waiting waits for the next.
    let Some(n) = state.observer.observe_to_boundary(bytes) else {
        return sys::write_all(STDOUT, bytes);
    };
    // Up to the boundary, with the bell spliced in as one write.
    let head = &bytes[..n];
    if state.bell_pending {
        state.bell_pending = false;
        let mut buf = Vec::with_capacity(n + 1);
        buf.extend_from_slice(head);
        buf.push(0x07);
        sys::write_all(STDOUT, &buf)?;
    } else {
        sys::write_all(STDOUT, head)?;
    }
    // The held notes go here, between the two halves of the frame: on fd 2
    // rather than fd 1, so they cannot be part of that one write, but the
    // terminal sees them in the order they are written. The rest of the
    // frame follows and may open a sequence of its own; that is the next
    // note's problem, not this one's.
    notes::at_boundary(true);
    sys::write_all(STDOUT, &bytes[n..])?;
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

/// Serve one link: finish the handshake, then relay until something ends
/// it. The HELLO is the link's `pending`: written with the dial already
/// (acs-trw), or still owed by a connection the session menu opened.
/// `sent_size` is the terminal size that HELLO carried.
#[allow(clippy::too_many_arguments)]
fn serve(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    mut link: Link,
    early: Vec<u8>,
    handshake: Duration,
    timing: &mut Timing,
    mut sent_size: proto::WinSize,
    netwatch: Option<&crate::netwatch::NetWatch>,
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
    let mut out = std::mem::take(&mut link.pending);
    let mut buf = vec![0u8; 64 * 1024];
    // The detector is `state`'s: it carries over from the last link and from
    // the offline wait in between (DESIGN §6.1). The bell does not — one
    // waiting from an earlier link's command mode is moot.
    state.bell_pending = false;
    let mut welcomed = false;
    let mut exiting = false;
    let mut liveness = crate::reconnect::Liveness::new();
    // From here until this call returns, a note raised while the program
    // is halfway through a sequence waits for the frame that finishes it
    // (acs-z22). Dropped on every way out, which writes what is still
    // held: the notes that *end* a link — a protocol error, an ERROR
    // frame, the takeover — are raised in here too.
    let _notes = notes::Relaying::open();

    // Every way out of here that is not an exit code says *why*, because
    // the redial ends the shared master on some of them and not on others
    // (acs-n1m, DESIGN §7.1).
    let lost = |link: Link, why: LinkEnd| {
        link.close();
        Outcome::LinkLost(why)
    };

    loop {
        // Decode everything available first.
        loop {
            let msg = match dec.next_msg() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    // Bytes got here and could not be read as a frame. The
                    // connection carried them, so it is up; this channel's
                    // conversation is what is over.
                    note(&format!("protocol error: {e}"));
                    return lost(link, LinkEnd::Channel);
                }
            };
            liveness.heard();
            match msg {
                Msg::Welcome(w) if !welcomed => {
                    welcomed = true;
                    timing.mark("WELCOME received");
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
                        // Neither of these two streams carries on where it
                        // stopped, so a sequence left unfinished is ended
                        // and not re-opened (acs-p4u): another program is
                        // talking now, or the same one across a gap whose
                        // next byte may start anywhere. The status line put
                        // the terminal back inside it, so the end of it is
                        // written here even when nothing else is.
                        AttachKind::Fresh => {
                            // Another program: undo the modes the last one
                            // left on before forgetting them, or leave()
                            // could not (acs-xk4).
                            let mut out = state.observer.interrupt_sequence().to_vec();
                            out.extend_from_slice(&state.observer.reset_sequence());
                            if !w.created {
                                // dtach's attach: clear, the program redraws.
                                out.extend_from_slice(b"\x1b[H\x1b[J");
                            }
                            if !out.is_empty() {
                                let _ = sys::write_all(STDOUT, &out);
                            }
                            state.observer.clear();
                        }
                        AttachKind::Gap => {
                            // The same program, still in its modes: keep
                            // them for leave(), but not a half-seen sequence.
                            let mut out = state.observer.interrupt_sequence().to_vec();
                            out.extend_from_slice(b"\x1b[H\x1b[J");
                            let _ = sys::write_all(STDOUT, &out);
                            state.observer.resync();
                        }
                    }
                    state.offset = w.offset;
                    state.attached_once = true;
                    // Spent (acs-y5r). The agreement was about whoever was
                    // attached a moment ago; hours later a redial may find
                    // somebody else there, and that is a new question.
                    state.force_next = false;
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
                    // The size went out with the HELLO, which now leaves
                    // before ssh has even connected (acs-trw): a window
                    // resized while the handshake was in flight raised a
                    // SIGWINCH that no RESIZE followed, since only a
                    // welcomed link sends them. Catch it up here, so the
                    // program starts on the size the terminal has.
                    if let Ok(s) = sys::get_winsize(STDIN) {
                        if s != sent_size {
                            sent_size = s;
                            Msg::Resize(s).encode(&mut out);
                        }
                    }
                    crate::reconnect::on_welcome(state, w.kind, &mut out);
                }
                Msg::Busy { identity, since } if !welcomed => {
                    match crate::reconnect::ask_takeover(state, &identity, since) {
                        true => {
                            // The question took the user's time, not the host's.
                            handshake_until = sys::now_ms() + handshake.as_millis() as u64;
                            state.force_next = true;
                            let again = hello(args, state, true);
                            sent_size = again.size;
                            Msg::Hello(again).encode(&mut out)
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
                        timing.first_output();
                        if write_output(state, &bytes[skip..]).is_err() {
                            link.close();
                            return Outcome::Exit(code::ERROR);
                        }
                        state.offset = offset + bytes.len() as u64;
                    }
                }
                Msg::Ack { seq } => state.unacked.ack(seq),
                // The answer to a question a network change asked, and the
                // only thing that is (acs-br2): bytes the host wrote before
                // the network moved are not evidence the link survived it.
                Msg::Pong(n) => liveness.pong(n),
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
            return lost(link, LinkEnd::Transport);
        }

        let now = sys::now_ms();
        // A note waiting for the program to finish its sequence is not
        // waiting for ever: a program that stopped mid-sequence, or a link
        // that died there, must not swallow it (acs-z22).
        notes::flush_due(now);
        let mut timeout: i64 = -1;
        let mut consider = |deadline: u64| {
            let d = deadline.saturating_sub(now) as i64;
            timeout = if timeout < 0 { d } else { timeout.min(d) };
        };
        if let Some(d) = state.detector.deadline() {
            consider(d);
        }
        if let Some(d) = notes::deadline() {
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
            sys::pollfd(netwatch.map(|w| w.fd()).unwrap_or(-1), libc::POLLIN),
        ];
        if sys::poll(&mut fds, timeout.min(i32::MAX as i64) as i32).is_err() {
            return lost(link, LinkEnd::Transport);
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

        // This machine's network moved under a link that is still up
        // (acs-ft1). The drain is unconditional — the descriptor has to be
        // emptied or the poll it woke would never sleep again — and a hint
        // that changed no network is not a change at all (acs-6p8).
        //
        // What it buys is a question, not a redial: the host is pinged now
        // and has `ACS_NETCHECK_MS` to answer, so a link that survived the
        // change costs nothing and one that did not is replaced in about a
        // second rather than after ten of silence.
        //
        // Before the WELCOME the handshake deadline is already the one that
        // applies, so the change is read and nothing is done with it —
        // acting on it there is acs-xo1, which has the blocking dial to
        // deal with as well. It is *not* spent by being read (acs-0n8):
        // the change is dropped rather than accepted, so the networks held
        // stay where they were and the kernel's next word about the same
        // move is the same change again, instead of "not a change" against
        // a set that has already moved.
        if fds[4].revents != 0 {
            if let Some(change) = netwatch.and_then(|w| w.changed()) {
                if welcomed {
                    liveness.netcheck(&mut out);
                    change.acted();
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
                    let was = state.detector.armed();
                    let o = state.detector.feed(&buf[..n], sys::now_ms());
                    follow_detector(state, was);
                    queue_input(state, &mut out, &o.forward);
                    action = o.action;
                }
            }
        } else if state
            .detector
            .deadline()
            .is_some_and(|d| sys::now_ms() >= d)
        {
            let was = state.detector.armed();
            let o = state.detector.tick(sys::now_ms());
            follow_detector(state, was);
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
                // This connection is the attached one, so the master lets
                // the kill through on that ground; the identity goes along
                // to name who asked (acs-fbo).
                Msg::Kill {
                    identity: identity(),
                    force: true,
                }
                .encode(&mut out);
                exiting = true;
            }
            None => {}
        }

        if fds[0].revents != 0 {
            match sys::read(from, &mut buf) {
                Ok(0) => return lost(link, LinkEnd::Transport),
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
                Err(_) => return lost(link, LinkEnd::Transport),
            }
        }

        if welcomed {
            match liveness.tick(&mut out) {
                crate::reconnect::Health::Ok => {}
                crate::reconnect::Health::Dead => return lost(link, LinkEnd::Silent),
            }
        } else if sys::now_ms() >= handshake_until {
            // Accepted, then silent: a redial goes back to its backoff; a
            // first connection gives up.
            if state.instance.is_some() {
                return lost(link, LinkEnd::Silent);
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

    /// acs-n1m: which endings take the shared ssh master down with them.
    /// A channel that broke while the connection was demonstrably carrying
    /// bytes does not — ending the master there drops every other acs
    /// session on it for nothing. Everything else does, including silence:
    /// a wedged master and a wedged network are the same evidence from
    /// here, and a stranded user is worse than a dropped sibling.
    #[test]
    fn only_an_ending_that_blames_the_connection_ends_the_master() {
        assert!(LinkEnd::Transport.failed_the_connection());
        assert!(LinkEnd::Silent.failed_the_connection());
        assert!(!LinkEnd::Channel.failed_the_connection());
        // No reason at all is the blunt side too: `connect_and_serve` ends
        // the master when it is given `None`.
        let unknown: Option<LinkEnd> = None;
        assert!(unknown.map_or(true, LinkEnd::failed_the_connection));
    }

    /// acs-z22: a note goes where the bell goes — at a boundary of the
    /// program's stream — and only while a frame loop is relaying one.
    ///
    /// The delay is the whole risk of this: a note about a dial that is
    /// still hanging is worth nothing once the dial has finished. So the
    /// gate holds a note in exactly one case, and this says which.
    #[test]
    fn a_note_waits_for_a_boundary_only_while_a_frame_loop_is_relaying() {
        notes::capture();
        // Outside the frame loop — the dial, the offline wait, alias
        // resolution — there is no frame in flight and nothing waits.
        note("dialling");
        assert_eq!(notes::written(), ["acs: dialling\r\n"]);

        let relaying = notes::Relaying::open();
        // Inside it, but between frames (and before the first one), the
        // stream is at a boundary: still nothing waits.
        note("between frames");
        assert_eq!(notes::written(), ["acs: between frames\r\n"]);

        // A frame that ended halfway through an escape sequence.
        notes::at_boundary(false);
        note("first");
        note("second");
        assert!(notes::written().is_empty(), "written into the sequence");
        // The frame that finishes it releases both, oldest first.
        notes::at_boundary(true);
        assert_eq!(notes::written(), ["acs: first\r\n", "acs: second\r\n"]);

        // And nothing is held behind them afterwards.
        note("after");
        assert_eq!(notes::written(), ["acs: after\r\n"]);
        drop(relaying);
        note("outside again");
        assert_eq!(notes::written(), ["acs: outside again\r\n"]);
    }

    /// acs-z22: what is held is delayed, never dropped. A program that
    /// stops mid-sequence, or a link that dies there, must not swallow a
    /// note — which is what makes the wait bounded rather than open-ended.
    #[test]
    fn a_held_note_survives_a_program_that_stops_mid_sequence() {
        notes::capture();
        let relaying = notes::Relaying::open();
        notes::at_boundary(false);
        note("held");
        // Nothing is due while the bytes that would finish the sequence
        // may still be on their way.
        let raised = sys::now_ms();
        notes::flush_due(raised);
        assert!(notes::written().is_empty());
        assert!(notes::deadline().is_some_and(|d| d > raised));
        // The sequence never finishes: `ACS_NOTE_HOLD_MS` later the note
        // is written where it stands rather than lost.
        notes::flush_due(raised + 60_000);
        assert_eq!(notes::written(), ["acs: held\r\n"]);
        assert_eq!(notes::deadline(), None);

        // The way out of the frame loop writes what is still held, so the
        // notes that end a link — a protocol error, an ERROR frame, the
        // takeover — arrive even when the last frame left a sequence open.
        notes::at_boundary(false);
        note("on the way out");
        assert!(notes::written().is_empty());
        drop(relaying);
        assert_eq!(notes::written(), ["acs: on the way out\r\n"]);
    }

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
