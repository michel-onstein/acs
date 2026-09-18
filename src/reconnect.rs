//! Keeping a session across links (DESIGN §5.2–§5.4): liveness, redial with
//! backoff, what the terminal shows while disconnected, and command keys
//! that still work when the link is down.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::cli::ClientArgs;
use crate::client::{self, code, note, Outcome, State};
use crate::keys::{Action, Detector};
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

/// Serve the session over as many links as it takes.
pub fn run(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
) -> u8 {
    let mut backoff = Backoff::new(env_ms("ACS_BACKOFF_MS", 1000));
    let mut resuming = false;
    loop {
        let started = Instant::now();
        match client::connect_and_serve(args, state, raw, signals, resuming) {
            Outcome::Exit(c) => return c,
            Outcome::LinkLost => {}
        }
        let name = state.session.clone().unwrap_or_default();
        if !args.reconnect || state.instance.is_none() {
            client::leave(state, raw);
            note(&format!(
                "connection lost — the session keeps running; reattach with: acs {} {name}",
                state.host
            ));
            return code::UNREACHABLE;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            backoff.reset();
        }
        let wait = backoff.step();
        match offline(state, raw, signals, wait) {
            Offline::Retry => resuming = true,
            Offline::Detach => {
                clear_status(state);
                client::leave(state, raw);
                note(&format!(
                    "detached from {}/{name} — reattach with: acs {} {name}",
                    state.host, state.host
                ));
                return code::DETACHED;
            }
        }
    }
}

enum Offline {
    Retry,
    Detach,
}

/// Wait `wait` before the next redial with the link down: show the status,
/// drop typed keys, but honour the command keys.
fn offline(
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
    wait: Duration,
) -> Offline {
    if let Some(r) = raw.as_mut() {
        let _ = r.resume();
    }
    show_status(
        state,
        &format!(
            "connection lost — reconnecting in {}s (Ctrl-] Ctrl-] d to detach)",
            wait.as_secs().max(1)
        ),
    );
    let deadline = Instant::now() + wait;
    let mut detector = Detector::new(client_escape());
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Offline::Retry;
        }
        let mut timeout = deadline - now;
        if let Some(d) = detector.deadline() {
            timeout = timeout.min(Duration::from_millis(d.saturating_sub(sys::now_ms())));
        }
        let mut fds = [
            sys::pollfd(0, libc::POLLIN),
            sys::pollfd(signals.as_raw_fd(), libc::POLLIN),
        ];
        let _ = sys::poll(&mut fds, timeout.as_millis() as i32);
        if fds[1].revents != 0 {
            sys::signals::drain(signals.as_raw_fd());
        }
        let out = if fds[0].revents != 0 {
            match sys::read(0, &mut buf) {
                Ok(n) if n > 0 => detector.feed(&buf[..n], sys::now_ms()),
                _ => return Offline::Detach,
            }
        } else {
            detector.tick(sys::now_ms())
        };
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

fn client_escape() -> crate::keys::Config {
    let mut cfg = crate::keys::Config::default();
    if let Some(k) = std::env::var("ACS_ESCAPE_KEY")
        .ok()
        .and_then(|v| crate::keys::Config::parse_key(&v))
    {
        cfg.byte = k;
    }
    cfg
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
}

/// Take the title back (pop the stack) if we pushed one.
fn clear_status(state: &mut State) {
    if state.status_shown {
        let _ = sys::write_all(1, b"\x1b[23;0t");
        state.status_shown = false;
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

/// Another identity is attached: may we take over?
pub fn ask_takeover(_state: &mut State, identity: &str, _since: u64) -> bool {
    note(&format!(
        "the session is attached from {identity}; use --force to take it over"
    ));
    false
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
