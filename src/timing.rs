//! How long each phase of a connection took, under `-v` (acs-pgn): the
//! alias resolved, ssh spawned, the `ACS-READY` marker seen, the session
//! list (the menu's `_proxy --pick`), WELCOME, the first output byte. One
//! line per phase, for the first connection and for every redial, so it is
//! known what dominates on a real host before anything is optimised.

use std::time::{Duration, Instant};

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
    pub fn mark(&mut self, phase: &str) {
        if !self.on || self.done {
            return;
        }
        let now = Instant::now();
        crate::client::note(&line(self.what, phase, now - self.last, now - self.start));
        self.last = now;
    }

    /// The first output byte: the last phase, told once per connection.
    pub fn first_output(&mut self) {
        self.mark("first output byte");
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
    fn nothing_is_told_after_the_first_output_byte() {
        let mut t = Timing::start(false, "first connection");
        t.first_output();
        assert!(t.done);
        let mut t = Timing::start(true, "first connection");
        t.done = true;
        let before = t.last;
        t.mark("WELCOME received");
        assert_eq!(t.last, before, "a finished clock does not move");
    }
}
