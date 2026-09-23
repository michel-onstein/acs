//! Keeping a session across links (DESIGN §5.2–§5.4): liveness, redial with
//! backoff, what the terminal shows while disconnected, and command keys
//! that still work when the link is down.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::cli::ClientArgs;
use crate::client::{self, code, note, Outcome, State};
use crate::keys::Action;
use crate::proto::{AttachKind, Msg};
use crate::sys;
use crate::tty::RawMode;

fn env_ms(name: &str, default: u64) -> u64 {
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

/// Exponential backoff between redials: 1 s → 30 s (`ACS_BACKOFF_MS` sets
/// the first step), reset after a connection that lasted 30 s.
pub struct Backoff {
    first: u64,
    next: u64,
}

impl Backoff {
    pub fn new(first_ms: u64) -> Backoff {
        Backoff {
            first: first_ms,
            next: first_ms,
        }
    }

    pub fn reset(&mut self) {
        self.next = self.first;
    }

    pub fn step(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(30_000.max(self.first));
        Duration::from_millis(d)
    }
}

/// Serve the session over as many links as it takes, the first one the
/// session menu's when it opened one (`picked`, DESIGN §4.4).
pub fn run(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    mut picked: Option<client::Picked>,
) -> u8 {
    let mut backoff = Backoff::new(env_ms("ACS_BACKOFF_MS", 1000));
    let netwatch = crate::netwatch::NetWatch::new();
    let mut last_early: Option<Instant> = None;
    let mut resuming = false;
    // A dial after a wait; `gated`: the host has just answered the ping
    // gate, so it is resolved already.
    let mut redial = false;
    let mut gated = false;
    let mut args = args.clone();
    loop {
        let started = Instant::now();
        // An alias is resolved again for every redial, so a fallback host is
        // picked up after a network change (DESIGN §7.3).
        let reachable = !redial || gated || redial_alias(&mut args, state);
        gated = false;
        let first = picked.take();
        let was_picked = first.is_some();
        let outcome = if reachable {
            client::connect_and_serve(&args, state, raw, signals, resuming, first)
        } else {
            Outcome::LinkLost
        };
        match outcome {
            Outcome::Exit(c) => return c,
            // The menu's connection sat idle while the user read it, and may
            // have died meanwhile: before any session, dial it afresh once.
            Outcome::LinkLost if was_picked && state.instance.is_none() => continue,
            Outcome::LinkLost => {}
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
            let msg = format!(
                "connection lost — reconnecting in {}s ({})",
                wait.as_secs().max(1),
                give_up_keys(state)
            );
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
                    // A new network is a fresh start.
                    backoff.reset();
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

/// At most one early redial per this interval, however chatty the network.
const EARLY_EVERY: Duration = Duration::from_secs(2);

/// Wait `wait` before the next redial with the link down: show `msg` as the
/// status, drop typed keys, but honour the command keys.
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
        if fds[2].revents != 0 && netwatch.is_some_and(|w| w.changed()) {
            let quiet = last_early.map_or(true, |t| t.elapsed() >= EARLY_EVERY);
            if quiet {
                *last_early = Some(Instant::now());
                return Offline::NetworkChanged;
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
        }
    }

    /// A frame arrived.
    pub fn heard(&mut self) {
        self.heard = sys::now_ms();
    }

    pub fn next_deadline_ms(&self) -> u64 {
        let ping = self.heard.max(self.pinged) + self.ping_after;
        ping.min(self.heard + self.dead_after)
    }

    pub fn tick(&mut self, out: &mut Vec<u8>) -> Health {
        self.tick_at(sys::now_ms(), out)
    }

    fn tick_at(&mut self, now: u64, out: &mut Vec<u8>) -> Health {
        if now >= self.heard + self.dead_after {
            return Health::Dead;
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

    #[test]
    fn backoff_doubles_to_thirty_seconds_and_resets() {
        let mut b = Backoff::new(1000);
        let steps: Vec<u64> = (0..7).map(|_| b.step().as_millis() as u64).collect();
        assert_eq!(steps, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
        b.reset();
        assert_eq!(b.step(), Duration::from_millis(1000));
    }

    #[test]
    fn liveness_pings_then_declares_dead() {
        let mut l = Liveness {
            heard: 1000,
            pinged: 1000,
            ping_after: 100,
            dead_after: 300,
        };
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
