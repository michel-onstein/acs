//! How long each phase of a connection took, under `-v` (acs-pgn). One
//! line per phase, for the first connection and for every redial, so it is
//! known what dominates on a real host before anything is optimised.
//!
//! [`Phase`] is the one list of the phases (acs-fb7r): a connection can be
//! told nothing else, and `tests/timing.rs` checks DESIGN §7's list against
//! [`Phase::ALL`], so neither can grow without the other.

use std::time::{Duration, Instant};

/// `Phase`, its `ALL` and its `name` from one list, so a variant cannot be
/// added without its name or left out of `ALL`.
macro_rules! phases {
    ($($(#[$doc:meta])* $v:ident = $name:literal,)*) => {
        /// A phase of a connection, in the order a connection can go
        /// through them.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Phase {
            $($(#[$doc])* $v,)*
        }

        impl Phase {
            /// Every phase, in order.
            pub const ALL: &'static [Phase] = &[$(Phase::$v,)*];

            /// The name `-v` tells it by.
            pub fn name(self) -> &'static str {
                match self {
                    $(Phase::$v => $name,)*
                }
            }
        }
    };
}

phases! {
    /// An alias's pings and lookups are done.
    AliasResolved = "alias resolved",
    /// A dial or a handshake was abandoned for a network change (§7).
    NetworkMoved = "network moved — dialing again",
    /// acs's own ssh master may be joined (acs-9n3).
    MasterChecked = "shared ssh master checked",
    /// The transport is running.
    SshSpawned = "ssh spawned",
    /// Only part of the greeting went with the dial (acs-ftn).
    HelloPartlySent = "HELLO partly sent",
    /// The whole greeting has gone (acs-ftn).
    HelloSent = "HELLO sent",
    /// The remote runs this acs.
    ReadySeen = "ACS-READY seen",
    /// The remote has no acs, or not this one: an install follows.
    NeedSeen = "ACS-NEED seen",
    /// The install on the same connection is done (acs-6k2).
    Installed = "acs installed",
    /// The menu's `_proxy --pick` answered (§4.4).
    SessionListReceived = "session list received",
    /// The session's WELCOME.
    WelcomeReceived = "WELCOME received",
    /// The first output byte: the last phase told.
    FirstOutput = "first output byte",
}

/// The clock of one connection attempt. Silent unless `-v`.
pub struct Timing {
    on: bool,
    /// Which connection: `first connection`, `redial`.
    what: &'static str,
    start: Instant,
    last: Instant,
    /// The first output byte is the last phase told; after it, nothing is.
    done: bool,
}

impl Timing {
    /// Start the clock of a connection attempt now; `on` under `-v`.
    pub fn start(on: bool, what: &'static str) -> Timing {
        let now = Instant::now();
        Timing {
            on,
            what,
            start: now,
            last: now,
            done: false,
        }
    }

    /// A clock that never tells anything: connections whose phases are not
    /// reported (`acs list`, a menu over every host).
    pub fn off() -> Timing {
        Timing::start(false, "")
    }

    /// `phase` has just been reached: tell how long it took since the last
    /// one, and since the start.
    pub fn mark(&mut self, phase: Phase) {
        if !self.on || self.done {
            return;
        }
        let now = Instant::now();
        let told = line(self.what, phase.name(), now - self.last, now - self.start);
        crate::client::note(&told);
        self.last = now;
    }

    /// The greeting has gone into the transport (acs-ftn).
    ///
    /// `all` is whether the whole of it went: the dial writes the HELLO
    /// into ssh's stdin the moment ssh is spawned (DESIGN §5.3, acs-trw)
    /// and that write is non-blocking, so a pipe that would not take all
    /// of it leaves the rest in `Link::pending` for `serve` to write —
    /// `HELLO partly sent` there, and `HELLO sent` again where the last of
    /// it finally goes. The menu's connection sends one ahead too
    /// (acs-xvg); when a session is detached that one is moot, and the
    /// greeting of the session picked is told again, late, as it goes.
    ///
    /// So `HELLO sent` always means the whole greeting is out, wherever in
    /// the order it lands: before `ACS-READY seen` when it went with the
    /// dial, which is the point of acs-trw, and after it when it did not.
    pub fn hello(&mut self, all: bool) {
        self.mark(match all {
            true => Phase::HelloSent,
            false => Phase::HelloPartlySent,
        });
    }

    /// The first output byte: the last phase, told once per connection.
    pub fn first_output(&mut self) {
        self.mark(Phase::FirstOutput);
        self.done = true;
    }
}

/// `timing: <what>: <phase> +<since last> ms (<since start> ms total)`.
fn line(what: &str, phase: &str, step: Duration, total: Duration) -> String {
    format!(
        "timing: {what}: {phase} +{} ms ({} ms total)",
        step.as_millis(),
        total.as_millis()
    )
}

/// One timing line read back: what [`line`] wrote.
#[doc(hidden)]
#[derive(Debug, PartialEq, Eq)]
pub struct Told<'a> {
    pub what: &'a str,
    pub phase: &'a str,
    pub step_ms: u64,
    pub total_ms: u64,
}

/// Every timing line in `text`, in order, wherever on a line it starts:
/// the tests' one parser of the format (`tests/timing.rs`,
/// `tests/e2e_ssh.rs`). A line that does not end as [`line`] ends it is
/// not one.
#[doc(hidden)]
pub fn told(text: &str) -> Vec<Told<'_>> {
    text.split("timing: ")
        .skip(1)
        .filter_map(|rest| {
            let rest = rest.split(['\r', '\n']).next()?;
            let (what, rest) = rest.split_once(": ")?;
            let (phase, rest) = rest.rsplit_once(" +")?;
            let (step, rest) = rest.split_once(" ms (")?;
            let total = rest.strip_suffix(" ms total)")?;
            Some(Told {
                what,
                phase,
                step_ms: step.parse().ok()?,
                total_ms: total.parse().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_names_the_connection_the_phase_and_both_durations() {
        assert_eq!(
            line(
                "redial",
                "WELCOME received",
                Duration::from_micros(12_700),
                Duration::from_millis(340)
            ),
            "timing: redial: WELCOME received +12 ms (340 ms total)"
        );
    }

    #[test]
    fn a_line_reads_back_as_it_was_written() {
        let ms = Duration::from_millis;
        let text = format!(
            "acs: {}\r\nnoise\nacs: {}\r\nacs: timing: not one\r\n",
            line(
                "first connection",
                Phase::NetworkMoved.name(),
                ms(3),
                ms(40)
            ),
            line("redial", Phase::ReadySeen.name(), ms(12), ms(52)),
        );
        assert_eq!(
            told(&text),
            [
                Told {
                    what: "first connection",
                    phase: "network moved — dialing again",
                    step_ms: 3,
                    total_ms: 40
                },
                Told {
                    what: "redial",
                    phase: "ACS-READY seen",
                    step_ms: 12,
                    total_ms: 52
                },
            ]
        );
    }

    #[test]
    fn every_phase_has_a_name_of_its_own() {
        let mut names: Vec<_> = Phase::ALL.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Phase::ALL.len());
    }

    #[test]
    fn the_greeting_is_named_whole_or_partly_sent() {
        crate::client::capture_notes();
        let mut t = Timing::start(true, "first connection");
        t.hello(false);
        t.hello(true);
        let told = crate::client::captured_notes();
        assert!(
            told[0].contains("first connection: HELLO partly sent +"),
            "{told:?}"
        );
        assert!(
            told[1].contains("first connection: HELLO sent +"),
            "{told:?}"
        );
    }

    #[test]
    fn nothing_is_told_after_the_first_output_byte() {
        let mut t = Timing::start(false, "first connection");
        t.first_output();
        assert!(t.done);
        let mut t = Timing::start(true, "first connection");
        t.done = true;
        let before = t.last;
        t.mark(Phase::WelcomeReceived);
        assert_eq!(t.last, before, "a finished clock does not move");
    }
}
