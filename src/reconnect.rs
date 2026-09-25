//! Keeping a session across links (DESIGN §5.2–§5.4): liveness, redial at
//! once and then with backoff, what the terminal shows while disconnected,
//! and command keys that still work when the link is down.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::cli::ClientArgs;
use crate::client::{self, code, note, LinkEnd, Outcome, State};
use crate::keys::Action;
use crate::proto::{AttachKind, Msg};
use crate::sys;
use crate::timing::Timing;
use crate::tty::RawMode;

pub(crate) fn env_ms(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Send a PING after this much silence (`ACS_PING_MS`).
fn ping_after() -> u64 {
    env_ms("ACS_PING_MS", 3000)
}

/// Declare the link dead after this much silence (`ACS_DEAD_MS`).
fn dead_after() -> u64 {
    env_ms("ACS_DEAD_MS", 10_000)
}

/// How long the host has to answer the PING a network change sends while
/// the link is up (`ACS_NETCHECK_MS`, acs-ft1). Short, because the question
/// is only "is this link still the one" and the answer is one round trip;
/// the full `ACS_DEAD_MS` stands for every other kind of silence.
fn netcheck_after() -> u64 {
    env_ms("ACS_NETCHECK_MS", 2000)
}

/// Waits between redials (acs-iyq): **the first attempt after a drop goes
/// at once**, and only then the exponential backoff 1 s → 30 s
/// (`ACS_BACKOFF_MS` sets its first step), reset after a connection that
/// lasted 30 s.
///
/// Most drops are momentary — a Wi-Fi blip, a laptop waking, a NAT that
/// forgot the flow — and the link is back by the time the drop is noticed
/// at all (10 s of silence, §5.3). Waiting a second more before even
/// looking put that second on every resume to buy nothing; a host that is
/// really gone answers the free attempt with a refused connect and the
/// backoff starts from there, so nobody is hammered either.
pub struct Backoff {
    first: u64,
    /// The next wait, or `None` for the free attempt that goes at once.
    next: Option<u64>,
}

impl Backoff {
    pub fn new(first_ms: u64) -> Backoff {
        Backoff {
            first: first_ms,
            next: None,
        }
    }

    /// Back to the free immediate attempt: this is a fresh drop, not the
    /// same one being sat out.
    pub fn reset(&mut self) {
        self.next = None;
    }

    /// A redial is being made right now for a reason of its own (a network
    /// change): that *is* the immediate attempt, so what follows it is the
    /// base wait rather than a second dial with nothing in between.
    pub fn spent(&mut self) {
        self.next = Some(self.first);
    }

    pub fn step(&mut self) -> Duration {
        let d = self.next.unwrap_or(0);
        self.next = Some(match self.next {
            None => self.first,
            Some(n) => (n * 2).min(30_000.max(self.first)),
        });
        Duration::from_millis(d)
    }
}

/// Serve the session over as many links as it takes, the first one the
/// session menu's when it opened one (`picked`, DESIGN §4.4). `timing` is
/// the first connection's clock; every redial starts one of its own.
pub fn run(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    mut picked: Option<client::Picked>,
    mut timing: Timing,
) -> u8 {
    let mut backoff = Backoff::new(env_ms("ACS_BACKOFF_MS", 1000));
    let netwatch = crate::netwatch::NetWatch::new(args.verbose > 0);
    let mut last_early: Option<Instant> = None;
    let mut resuming = false;
    // A dial after a wait; `gated`: the host has just answered the ping
    // gate, so it is resolved already.
    let mut redial = false;
    let mut gated = false;
    // How the last link ended, carried to the next dial so it knows
    // whether the connection it ran on failed or only our channel on it
    // did (acs-n1m). `None` until a link has ended.
    let mut ended: Option<LinkEnd> = None;
    let mut args = args.clone();
    loop {
        let started = Instant::now();
        if redial {
            timing = Timing::start(args.verbose > 0, "redial");
        }
        // An alias is resolved again for every redial, so a fallback host is
        // picked up after a network change (DESIGN §7.3).
        let reachable = !redial || gated || redial_alias(&mut args, state);
        if redial && reachable && !gated && args.alias.is_some() {
            timing.mark("alias resolved");
        }
        gated = false;
        let first = picked.take();
        let was_picked = first.is_some();
        let outcome = if reachable {
            client::connect_and_serve(
                &args,
                state,
                raw,
                signals,
                resuming,
                ended,
                first,
                &mut timing,
                netwatch.as_ref(),
            )
        } else {
            // No host to dial: nothing new was learned about the link that
            // was lost, so its reason still stands. (`ended` is set by
            // then — this branch only runs on a redial.)
            Outcome::LinkLost(ended.unwrap_or(LinkEnd::Transport))
        };
        match outcome {
            Outcome::Exit(c) => return c,
            // The menu's connection sat idle while the user read it, and may
            // have died meanwhile: before any session, dial it afresh once.
            Outcome::LinkLost(e) if was_picked && state.instance.is_none() => {
                ended = Some(e);
                continue;
            }
            Outcome::LinkLost(e) => ended = Some(e),
        }
        let name = state.session.clone().unwrap_or_default();
        // Persisting, even a host lost before any session is waited for
        // (DESIGN §5.3).
        let persist = client::persist(&args);
        if !args.reconnect || (state.instance.is_none() && !persist) {
            client::leave(state, raw);
            note(&format!(
                "connection lost — the session keeps running; reattach with: acs {} {name}",
                state.host
            ));
            return code::UNREACHABLE;
        }
        redial = true;
        let detached = if persist && pingable(&args) {
            // The ping gate: wait for the host to answer, then dial.
            match wait_for_answer(
                &mut args,
                state,
                raw,
                signals,
                netwatch.as_ref(),
                &mut last_early,
            ) {
                true => {
                    gated = true;
                    false
                }
                false => true,
            }
        } else {
            if started.elapsed() >= Duration::from_secs(30) {
                backoff.reset();
            }
            let wait = backoff.step();
            // The status line goes up even when nothing is waited for: the
            // dial itself takes as long as it takes, and it is what the
            // frozen terminal is owed an explanation for. It is also what
            // `on_welcome` repaints over afterwards.
            let keys = give_up_keys(state);
            let msg = match wait.is_zero() {
                true => format!("connection lost — reconnecting now ({keys})"),
                false => format!(
                    "connection lost — reconnecting in {}s ({keys})",
                    wait.as_secs().max(1),
                ),
            };
            match offline(
                state,
                raw,
                signals,
                wait,
                netwatch.as_ref(),
                &mut last_early,
                &msg,
            ) {
                Offline::Retry => false,
                Offline::NetworkChanged => {
                    // A new network is a fresh start — and the redial it
                    // cuts the wait short for is the immediate attempt
                    // itself, so the backoff starts at its base again
                    // rather than handing out a second dial for free.
                    backoff.spent();
                    false
                }
                Offline::Detach => true,
            }
        };
        // A session to resume, or (persisting) still the first attach.
        resuming = state.instance.is_some();
        if detached {
            clear_status(state);
            client::leave(state, raw);
            if state.instance.is_none() {
                note(&format!("stopped waiting for {}", state.host));
                return code::UNREACHABLE;
            }
            note(&format!(
                "detached from {}/{name} — reattach with: acs {} {name}",
                state.host, state.host
            ));
            return code::DETACHED;
        }
    }
}

/// How to stop waiting, for the status line: detach a session, or, before
/// there is one (the terminal still cooked), give up.
fn give_up_keys(state: &State) -> &'static str {
    match state.instance {
        Some(_) => "Ctrl-] Ctrl-] d to detach",
        None => "Ctrl-C gives up",
    }
}

/// Whether the lost host can be pinged for the gate: a plain host always,
/// an alias if any of its hosts has `reachability_check` on (the others
/// keep the dial backoff).
fn pingable(args: &ClientArgs) -> bool {
    let Some(name) = &args.alias else {
        return true;
    };
    args.config
        .alias(crate::alias::split_user(name).1)
        .map_or(true, |a| a.entries.iter().any(|e| e.reachability_check))
}

/// Persisting (DESIGN §5.3): ping the lost host every
/// `reachability_interval` — at once on a network change — until it
/// answers (`true`: dial it now), showing the status line and honouring
/// the command keys meanwhile (`false`: the user detached, or gave up).
/// An alias is resolved again each time, so its first host to answer is
/// the one dialled.
fn wait_for_answer(
    args: &mut ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    netwatch: Option<&crate::netwatch::NetWatch>,
    last_early: &mut Option<Instant>,
) -> bool {
    let every = client::reachability_interval(args);
    let msg = format!(
        "{} is not answering — pinging it every {} ({})",
        state.host,
        crate::config::format_timeout(every),
        give_up_keys(state)
    );
    loop {
        match offline(state, raw, signals, every, netwatch, last_early, &msg) {
            Offline::Retry | Offline::NetworkChanged => {
                let answers = match args.alias.clone() {
                    Some(_) => redial_alias(args, state),
                    None => {
                        let name = args.transport.destination.clone();
                        client::host_answers(args, &name) != Some(false)
                    }
                };
                if answers {
                    return true;
                }
            }
            Offline::Detach => return false,
        }
    }
}

/// Re-resolve the alias before a redial; false if no host answers now. A
/// change of host is always told: the session is only there if the new host
/// is the same machine.
fn redial_alias(args: &mut crate::cli::ClientArgs, state: &mut State) -> bool {
    let Some(name) = args.alias.clone() else {
        return true;
    };
    let before = args.transport.destination.clone();
    match client::resolve_alias(args, &name) {
        Ok(()) => {
            if args.transport.destination != before {
                clear_status(state);
                note(&format!(
                    "{name}: now using {} (was {before})",
                    args.transport.destination
                ));
            }
            true
        }
        Err(e) => {
            if args.verbose > 0 {
                note(&e);
            }
            false
        }
    }
}

enum Offline {
    Retry,
    NetworkChanged,
    Detach,
}

/// At most one early redial per this interval (`ACS_EARLY_MS`). A chatty
/// network is no longer what this guards against — the watcher answers a
/// hint that changed nothing with `None` (acs-6p8) — but a link coming up
/// and going down again can change the machine's addresses twice in a
/// second, and dialling on each would drop what was typed in between for
/// nothing.
///
/// A change suppressed here is **not spent** (acs-0n8): it is dropped
/// rather than accepted, so the watcher still holds the networks of the
/// last redial and the kernel's next word about the flap is the same
/// change again. What that is not is a queue. Nothing fires without a
/// hint, and a hint is one comparison against the networks of the moment
/// however many changes went by — so a flapping interface still costs one
/// redial per interval, exactly as it did when the change was thrown away.
/// What it buys is that the change is no longer lost to the machine
/// settling quietly: any later hint carries it, instead of the backoff
/// (30 s at worst) having to cover for it.
fn early_every() -> Duration {
    Duration::from_millis(env_ms("ACS_EARLY_MS", 2000))
}

/// Wait `wait` before the next redial with the link down: show `msg` as the
/// status, drop typed keys, but honour the command keys. A `wait` of zero
/// (the free first attempt, [`Backoff`]) puts the status up and returns at
/// once, so the dial follows the drop with nothing in between.
///
/// The detector is the client's own (`state.detector`, DESIGN §6.1), not one
/// of this wait's: that makes it the same configuration as online, window
/// included (acs-wxa), and it leaves a half-finished double tap held when
/// the backoff expires under it, so the redial attempt in between does not
/// eat the first press (acs-e80).
fn offline(
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    wait: Duration,
    netwatch: Option<&crate::netwatch::NetWatch>,
    last_early: &mut Option<Instant>,
    msg: &str,
) -> Offline {
    if let Some(r) = raw.as_mut() {
        let _ = r.resume();
    }
    show_status(state, msg);
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Offline::Retry;
        }
        let mut timeout = deadline - now;
        if let Some(d) = state.detector.deadline() {
            timeout = timeout.min(Duration::from_millis(d.saturating_sub(sys::now_ms())));
        }
        let mut fds = [
            sys::pollfd(0, libc::POLLIN),
            sys::pollfd(signals.as_raw_fd(), libc::POLLIN),
            sys::pollfd(netwatch.map(|w| w.fd()).unwrap_or(-1), libc::POLLIN),
        ];
        let _ = sys::poll(&mut fds, timeout.as_millis() as i32);
        if fds[1].revents != 0 {
            sys::signals::drain(signals.as_raw_fd());
        }
        if fds[2].revents != 0 {
            if let Some(change) = netwatch.and_then(|w| w.changed()) {
                let quiet = last_early.map_or(true, |t| t.elapsed() >= early_every());
                if quiet {
                    // Accepted, so the networks it was judged against are
                    // these from now on; a rate-limited one is dropped
                    // instead and offered again (acs-0n8, [`early_every`]).
                    change.acted();
                    *last_early = Some(Instant::now());
                    return Offline::NetworkChanged;
                }
            }
        }
        let was = state.detector.armed();
        let out = if fds[0].revents != 0 {
            match sys::read(0, &mut buf) {
                Ok(n) if n > 0 => state.detector.feed(&buf[..n], sys::now_ms()),
                _ => return Offline::Detach,
            }
        } else {
            state.detector.tick(sys::now_ms())
        };
        client::follow_detector(state, was);
        // Keys typed into a dead link are dropped, not queued (DESIGN §5.2).
        match out.action {
            Some(Action::Detach) => return Offline::Detach,
            Some(Action::Exit) => {
                show_status(
                    state,
                    "ending the session needs the connection — Ctrl-] Ctrl-] d detaches instead",
                );
            }
            None => {}
        }
    }
}

/// One line on the bottom row (cursor saved and restored) and the window
/// title, pushed on the xterm title stack so the program's comes back.
fn show_status(state: &mut State, msg: &str) {
    let rows = sys::get_winsize(0).map(|s| s.rows).unwrap_or(24).max(1);
    let mut out = Vec::new();
    if !state.status_shown {
        out.extend_from_slice(b"\x1b[22;0t");
    }
    out.extend_from_slice(format!("\x1b]2;acs: {} — {msg}\x07", state.host).as_bytes());
    out.extend_from_slice(
        format!("\x1b7\x1b[{rows};1H\x1b[2K\x1b[7macs: {msg}\x1b[0m\x1b8").as_bytes(),
    );
    let _ = sys::write_all(1, &out);
    state.status_shown = true;
    crate::tty::set_status_shown(true);
}

/// Take the title back (pop the stack) and blank the status row, if we
/// drew them. Every way out of the client passes here (`client::leave`),
/// so a session that ends during an outage leaves no trace (acs-qrn).
pub fn clear_status(state: &mut State) {
    if state.status_shown {
        let rows = sys::get_winsize(0).map(|s| s.rows).unwrap_or(24).max(1);
        let out = format!("\x1b[23;0t\x1b7\x1b[{rows};1H\x1b[2K\x1b8");
        let _ = sys::write_all(1, out.as_bytes());
        state.status_shown = false;
        crate::tty::set_status_shown(false);
    }
}

/// A WELCOME arrived on a link. After a resume that followed a status line,
/// ask the program to repaint: two size changes (rows-1, then rows), since
/// an unchanged size sends no SIGWINCH.
pub fn on_welcome(state: &mut State, kind: AttachKind, out: &mut Vec<u8>) {
    if !state.status_shown {
        return;
    }
    clear_status(state);
    if kind == AttachKind::Resumed {
        if let Ok(s) = sys::get_winsize(0) {
            if s.rows > 1 {
                let mut smaller = s;
                smaller.rows -= 1;
                Msg::Resize(smaller).encode(out);
                Msg::Resize(s).encode(out);
            }
        }
    }
}

/// Another identity is attached (DESIGN §4.5): ask before taking over.
/// Runs in cooked mode (before raw mode, or while redialling), so the
/// answer is a line.
pub fn ask_takeover(state: &mut State, identity: &str, since: u64) -> bool {
    let name = state.session.clone().unwrap_or_default();
    if !sys::isatty(0) {
        note(&format!(
            "session '{name}' is attached from {identity}; use --force to take it over"
        ));
        return false;
    }
    // The identity comes from whoever is attached and is asserted, not
    // checked. This is a security question, so it must read as acs wrote
    // it: an identity that erases the line and prints its own question
    // would collect a "y" for something else entirely (acs-w1z).
    let question = crate::safe::display_max(
        &format!(
            "acs: session '{name}' on {} is attached from {identity} since {} — take over? [y/N] ",
            state.host,
            sys::local_hhmm(since)
        ),
        4 * crate::safe::MAX_FIELD,
    );
    // Keys typed before the question are not its answer (acs-qty).
    let _ = sys::flush_input(0);
    let _ = sys::write_all(2, question.as_bytes());
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while let Ok(1) = sys::read(0, &mut b) {
        if b[0] == b'\n' || b[0] == b'\r' {
            break;
        }
        line.push(b[0]);
    }
    matches!(line.first(), Some(b'y' | b'Y'))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    Dead,
}

/// Link liveness (DESIGN §5.3): PING after silence, dead after longer.
pub struct Liveness {
    heard: u64,
    pinged: u64,
    ping_after: u64,
    dead_after: u64,
    netcheck_after: u64,
    /// A network change is being asked about (acs-ft1): the nonce of the
    /// PING that asked, and when the link is dead if it has not been
    /// answered. `None` when no question is outstanding.
    probe: Option<(u64, u64)>,
}

impl Liveness {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Liveness {
        let now = sys::now_ms();
        Liveness {
            heard: now,
            pinged: now,
            ping_after: ping_after(),
            dead_after: dead_after(),
            netcheck_after: netcheck_after(),
            probe: None,
        }
    }

    /// A frame arrived.
    pub fn heard(&mut self) {
        self.heard = sys::now_ms();
    }

    /// This machine's network changed under a link that is still up
    /// (DESIGN §5.3, acs-ft1): ask the host now and give it
    /// `ACS_NETCHECK_MS` to answer instead of the whole dead interval.
    ///
    /// A link that came through the change answers, and the change costs
    /// nothing at all — not a redial, and so not a byte of what was typed.
    /// One that did not is replaced in about a second instead of ten, which
    /// is what a Wi-Fi switch and a laptop wake are. On a wake the ten
    /// would not even have started: the clock is monotonic and stops with
    /// the machine (`sys::now_ms`), so the silence the sleep contains is
    /// not silence anything measured.
    ///
    /// A second change while the question is outstanding is the same
    /// question — an interface flapping cannot push the deadline out, nor
    /// spend another round trip.
    ///
    /// **Only the PONG for this PING answers it** ([`Liveness::pong`],
    /// acs-br2): other bytes from the host do not, however fresh they look.
    /// Everywhere else in liveness a byte is proof the host is there, and
    /// here it is not — the frames already in the pipe when the network
    /// moved were written before it moved, so they are the frozen link's
    /// last gasp rather than an answer to a question asked after it. A
    /// starved client reads them in the same pass of its poll that sends
    /// the PING, and counting them left a link that was gone standing for
    /// the whole `ACS_DEAD_MS`, which is the one thing this deadline
    /// exists to avoid.
    pub fn netcheck(&mut self, out: &mut Vec<u8>) {
        self.netcheck_at(sys::now_ms(), out)
    }

    /// The host answered a PING with this nonce. It resolves an outstanding
    /// netcheck when it is that netcheck's own PING or a later one
    /// (acs-br2) — a PONG for a PING sent *before* the network moved
    /// travelled the old path and says nothing about the new one.
    pub fn pong(&mut self, nonce: u64) {
        if self.probe.is_some_and(|(asked, _)| nonce >= asked) {
            self.probe = None;
        }
    }

    fn netcheck_at(&mut self, now: u64, out: &mut Vec<u8>) {
        if self.probe.is_some() {
            return;
        }
        Msg::Ping(now).encode(out);
        self.pinged = now;
        self.probe = Some((now, now + self.netcheck_after));
    }

    pub fn next_deadline_ms(&self) -> u64 {
        let ping = self.heard.max(self.pinged) + self.ping_after;
        let next = ping.min(self.heard + self.dead_after);
        match self.probe {
            Some((_, until)) => next.min(until),
            None => next,
        }
    }

    pub fn tick(&mut self, out: &mut Vec<u8>) -> Health {
        self.tick_at(sys::now_ms(), out)
    }

    fn tick_at(&mut self, now: u64, out: &mut Vec<u8>) -> Health {
        if now >= self.heard + self.dead_after {
            return Health::Dead;
        }
        // A question a network change asked, and nothing answering it: the
        // link is gone. The answer is a PONG and nothing else
        // ([`Liveness::pong`], acs-br2).
        if let Some((_, until)) = self.probe {
            if now >= until {
                return Health::Dead;
            }
        }
        if now >= self.heard.max(self.pinged) + self.ping_after {
            Msg::Ping(now).encode(out);
            self.pinged = now;
        }
        Health::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// acs-iyq: the first attempt after a drop waits for nothing, and the
    /// doubling starts at `ACS_BACKOFF_MS` from the second.
    #[test]
    fn the_first_redial_goes_at_once_then_the_backoff_doubles_and_resets() {
        let mut b = Backoff::new(1000);
        let steps: Vec<u64> = (0..8).map(|_| b.step().as_millis() as u64).collect();
        assert_eq!(steps, [0, 1000, 2000, 4000, 8000, 16000, 30000, 30000]);
        // A connection that lasted, or a network change: free attempt again.
        b.reset();
        assert_eq!(b.step(), Duration::ZERO);
        assert_eq!(b.step(), Duration::from_millis(1000));
    }

    /// `ACS_BACKOFF_MS` still sets the base of the doubling — the free
    /// first attempt is not it (acs-iyq).
    #[test]
    fn the_configured_base_is_the_first_waited_step() {
        let mut b = Backoff::new(100);
        let steps: Vec<u64> = (0..4).map(|_| b.step().as_millis() as u64).collect();
        assert_eq!(steps, [0, 100, 200, 400]);
        // A base above the ceiling is honoured, as before: it is the wait a
        // test or a user asked for.
        let mut b = Backoff::new(120_000);
        assert_eq!(b.step(), Duration::ZERO);
        assert_eq!(b.step(), Duration::from_millis(120_000));
        assert_eq!(b.step(), Duration::from_millis(120_000));
    }

    /// A network change redials at once by itself (DESIGN §5.3), so it
    /// spends the free attempt rather than being given another one on top
    /// — two dials in a row with nothing in between would help nobody.
    #[test]
    fn a_redial_made_at_once_for_another_reason_spends_the_free_attempt() {
        let mut b = Backoff::new(1000);
        b.spent();
        assert_eq!(b.step(), Duration::from_millis(1000));
        assert_eq!(b.step(), Duration::from_millis(2000));
        // And it is the base that comes back, whatever the backoff had
        // climbed to while the network was down.
        b.spent();
        assert_eq!(b.step(), Duration::from_millis(1000));
    }

    fn liveness(heard: u64, ping_after: u64, dead_after: u64) -> Liveness {
        Liveness {
            heard,
            pinged: heard,
            ping_after,
            dead_after,
            netcheck_after: 50,
            probe: None,
        }
    }

    /// acs-ft1: a network change under a link that is still up asks the
    /// host at once and gives it `ACS_NETCHECK_MS` to answer. A link that
    /// answers costs nothing — no redial, so not a byte of what was typed;
    /// one that does not is dead well inside the dead interval.
    #[test]
    fn a_network_change_asks_the_host_and_shortens_the_deadline() {
        // Ten seconds of silence would be the ordinary death; the network
        // change's own deadline is 50 ms.
        let mut l = liveness(1000, 3000, 10_000);
        let mut out = Vec::new();
        l.netcheck_at(1100, &mut out);
        assert_eq!(out, Msg::Ping(1100).to_bytes(), "the host is asked at once");
        // The deadline the poll sleeps to is the change's, not the ping's.
        assert_eq!(l.next_deadline_ms(), 1150);
        // A second change while the question is outstanding is the same
        // question: no second round trip, and the deadline does not move.
        let n = out.len();
        l.netcheck_at(1140, &mut out);
        assert_eq!(out.len(), n, "a flapping interface asks once");
        assert_eq!(l.next_deadline_ms(), 1150);
        assert_eq!(l.tick_at(1149, &mut out), Health::Ok);
        assert_eq!(l.tick_at(1150, &mut out), Health::Dead);

        // And the link that answers — the PONG for that PING, which is what
        // answering means here (acs-br2): the deadline is forgotten and the
        // ordinary timers are back.
        let mut l = liveness(1000, 3000, 10_000);
        let mut out = Vec::new();
        l.netcheck_at(1100, &mut out);
        l.heard = 1120;
        l.pong(1100);
        assert_eq!(l.tick_at(1200, &mut out), Health::Ok);
        assert_eq!(l.probe, None);
        assert_eq!(l.next_deadline_ms(), 1120 + 3000);
        // Long past what the change's deadline would have been.
        assert_eq!(l.tick_at(5000, &mut out), Health::Ok);
        // The dead interval still applies, counted from the last frame.
        assert_eq!(l.tick_at(11_120, &mut out), Health::Dead);
    }

    /// acs-br2: the question a network change asks is answered by its own
    /// PONG and by nothing else. Frames that were already in the pipe when
    /// the network moved were written before it moved — a frozen link's last
    /// gasp — and an answer to a PING sent before it travelled the path
    /// that has gone.
    ///
    /// This is the bug the integration test of the same name reproduces.
    /// The client reads those frames in the same pass of its poll that
    /// sends the PING, so `heard` lands *after* the question was asked
    /// however stale the bytes are; taking that for an answer left a link
    /// that was already gone standing for the whole `ACS_DEAD_MS`, which is
    /// the one thing the change's deadline exists to prevent. It showed up
    /// as a test failing only on a cold, fully parallel container run,
    /// because an idle machine drains the pipe before the hint arrives.
    #[test]
    fn only_the_pong_for_its_own_ping_answers_a_network_change() {
        let mut l = liveness(1000, 3000, 10_000);
        let mut out = Vec::new();
        // A PING of the ordinary kind went out before the network moved.
        assert_eq!(l.tick_at(4000, &mut out), Health::Ok);
        assert_eq!(out, Msg::Ping(4000).to_bytes());
        // The network moves, and the question goes out: 50 ms to answer it.
        l.netcheck_at(4100, &mut out);
        // Bytes arrive — the frames the host wrote before it froze, read in
        // the same pass of the poll that sent the PING, so they are *newer*
        // than the question by the clock.
        l.heard = 4100;
        assert_eq!(l.tick_at(4149, &mut out), Health::Ok);
        assert_eq!(
            l.tick_at(4150, &mut out),
            Health::Dead,
            "bytes written before the network moved answered the netcheck"
        );
        // Nor does the PONG for the PING that went out before it: that one
        // came back over the path that has gone.
        let mut l = liveness(1000, 3000, 10_000);
        let mut out = Vec::new();
        assert_eq!(l.tick_at(4000, &mut out), Health::Ok);
        l.netcheck_at(4100, &mut out);
        l.pong(4000);
        l.heard = 4100;
        assert_eq!(l.tick_at(4150, &mut out), Health::Dead);
    }

    /// The change's deadline never outlives the dead interval: a machine
    /// configured with a longer `ACS_NETCHECK_MS` than `ACS_DEAD_MS` is
    /// still given up on after `ACS_DEAD_MS` of silence.
    #[test]
    fn the_dead_interval_is_the_outer_bound_whatever_the_change_asks_for() {
        let mut l = Liveness {
            netcheck_after: 60_000,
            ..liveness(1000, 3000, 300)
        };
        let mut out = Vec::new();
        l.netcheck_at(1050, &mut out);
        assert_eq!(l.tick_at(1300, &mut out), Health::Dead);
    }

    #[test]
    fn liveness_pings_then_declares_dead() {
        let mut l = liveness(1000, 100, 300);
        let mut out = Vec::new();
        // Not yet: nothing sent.
        assert_eq!(l.tick_at(1050, &mut out), Health::Ok);
        assert!(out.is_empty());
        assert_eq!(l.next_deadline_ms(), 1100);
        // After the ping interval a PING goes out, once per interval.
        assert_eq!(l.tick_at(1100, &mut out), Health::Ok);
        let n = out.len();
        assert!(n > 0);
        assert_eq!(l.tick_at(1150, &mut out), Health::Ok);
        assert_eq!(out.len(), n, "one ping per interval");
        assert_eq!(l.next_deadline_ms(), 1200);
        // A reply resets the clock.
        l.heard = 1180;
        assert_eq!(l.tick_at(1250, &mut out), Health::Ok);
        // Silence past the dead interval.
        assert_eq!(l.tick_at(1480, &mut out), Health::Dead);
    }
}
