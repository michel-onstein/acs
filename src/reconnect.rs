//! Keeping a session across links (DESIGN §5.3, §5.4). This first version
//! serves a single link: a lost link ends the client.

use std::os::fd::OwnedFd;

use crate::cli::ClientArgs;
use crate::client::{self, code, note, Outcome, State};
use crate::tty::RawMode;

/// Serve the session over as many links as it takes.
pub fn run(
    args: &ClientArgs,
    state: &mut State,
    raw: &mut Option<RawMode>,
    signals: &OwnedFd,
) -> u8 {
    match client::connect_and_serve(args, state, raw, signals, false) {
        Outcome::Exit(c) => c,
        Outcome::LinkLost => {
            client::leave(state, raw);
            let name = state.session.clone().unwrap_or_default();
            note(&format!(
                "connection lost — the session keeps running; reattach with: acs {} {name}",
                state.host
            ));
            code::UNREACHABLE
        }
    }
}

/// Called when a WELCOME arrives on a link.
pub fn on_welcome(_state: &mut State) {}

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

/// Link liveness (pings and the dead-link timeout).
pub struct Liveness;

impl Liveness {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Liveness {
        Liveness
    }
    pub fn heard(&mut self) {}
    pub fn next_deadline_ms(&self) -> u64 {
        u64::MAX
    }
    pub fn tick(&mut self, _out: &mut Vec<u8>) -> Health {
        Health::Ok
    }
}
