//! Command mode: Ctrl-] Ctrl-] followed by a command key (DESIGN §6).
//!
//! [`Detector`] is a pure state machine over stdin bytes and an injected
//! millisecond clock. It forwards everything unchanged except the escape
//! presses it consumes, recognising the escape key in all three encodings a
//! terminal may use (legacy byte, kitty `CSI … u`, xterm modifyOtherKeys),
//! never matching inside another escape sequence, and passing bracketed pastes
//! through untouched.

/// What a completed command asks the client to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the session running and exit the client.
    Detach,
    /// End the session on the remote and exit.
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The legacy control byte of the escape key (Ctrl-] = 0x1D).
    pub byte: u8,
    /// Max gap between the two escape presses.
    pub window_ms: u64,
    /// How long command mode waits for the command key.
    pub command_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            byte: 0x1d,
            window_ms: 400,
            command_timeout_ms: 2000,
        }
    }
}

impl Config {
    /// Codepoint kitty and modifyOtherKeys report for Ctrl+<this key>: the
    /// unshifted character (`^]` → `]`, `^A` → `a`).
    fn codepoint(&self) -> u32 {
        (self.byte ^ 0x40).to_ascii_lowercase() as u32
    }

    /// Parse `^]`-style notation (also `^A`…`^Z`, `^\`, `^^`, `^_`).
    pub fn parse_key(s: &str) -> Option<u8> {
        let b = s.as_bytes();
        if b.len() == 2 && b[0] == b'^' {
            let c = b[1].to_ascii_uppercase();
            if (b'@'..=b'_').contains(&c) && c != b'@' && c != b'[' {
                return Some(c ^ 0x40);
            }
        }
        None
    }
}

/// Result of feeding bytes or a clock tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub forward: Vec<u8>,
    pub action: Option<Action>,
}

/// How long an incomplete escape sequence is held before being sent as is.
const PENDING_MS: u64 = 100;
const MAX_SEQ: usize = 64;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Escape key pressed (or repeated).
    EscPress,
    /// Escape key released (kitty event types).
    EscRelease,
    /// Any other key; `Some(c)` when it is a plain character.
    Key(Option<u8>),
    /// A key release other than the escape key.
    Release,
    /// Terminal replies, mouse and focus reports: forwarded at once, never
    /// break a pending double tap.
    Passive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Ground,
    /// After ESC, waiting for the next byte.
    Esc,
    /// Collecting `ESC [` … final.
    Csi,
    /// `ESC O` needs one more byte.
    Ss3,
    /// Inside OSC/DCS/APC/PM/SOS until BEL or ST; `esc` = saw ESC.
    Str {
        esc: bool,
    },
    /// Legacy X10 mouse: raw bytes still to come after `CSI M`.
    Mouse(u8),
    /// Bracketed paste: bytes of PASTE_END matched so far.
    Paste(usize),
}

#[derive(Debug)]
enum State {
    Idle,
    /// One escape press held until `until`.
    Held {
        bytes: Vec<u8>,
        until: u64,
    },
    /// Double tap seen; waiting for the command key until `until`.
    Command {
        bytes: Vec<u8>,
        until: u64,
    },
}

pub struct Detector {
    cfg: Config,
    lex: Lex,
    /// Bytes of the sequence being lexed.
    seq: Vec<u8>,
    /// When an incomplete sequence started waiting.
    seq_since: Option<u64>,
    state: State,
}

impl Detector {
    pub fn new(cfg: Config) -> Self {
        Detector {
            cfg,
            lex: Lex::Ground,
            seq: Vec::new(),
            seq_since: None,
            state: State::Idle,
        }
    }

    /// True while command mode waits for its command key (after the double
    /// tap, before the key or the timeout).
    pub fn armed(&self) -> bool {
        matches!(self.state, State::Command { .. })
    }

    /// The earliest time [`Detector::tick`] must be called.
    pub fn deadline(&self) -> Option<u64> {
        let s = match &self.state {
            State::Idle => None,
            State::Held { until, .. } | State::Command { until, .. } => Some(*until),
        };
        let p = self.seq_since.map(|t| t + PENDING_MS);
        match (s, p) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Handle expired timers.
    pub fn tick(&mut self, now: u64) -> Output {
        let mut out = Output::default();
        if let Some(since) = self.seq_since {
            if now >= since + PENDING_MS {
                let bytes = std::mem::take(&mut self.seq);
                self.seq_since = None;
                self.lex = Lex::Ground;
                self.token(Kind::Key(None), bytes, now, &mut out);
            }
        }
        let expired = match &self.state {
            State::Held { until, .. } | State::Command { until, .. } => now >= *until,
            State::Idle => false,
        };
        if expired {
            if let State::Held { bytes, .. } | State::Command { bytes, .. } =
                std::mem::replace(&mut self.state, State::Idle)
            {
                out.forward.extend_from_slice(&bytes);
            }
        }
        out
    }

    /// Feed bytes read from stdin at time `now`.
    pub fn feed(&mut self, input: &[u8], now: u64) -> Output {
        let mut out = self.tick(now);
        let mut i = 0;
        let mut plain_start: Option<usize> = None;
        macro_rules! flush_plain {
            ($end:expr) => {
                if let Some(s) = plain_start.take() {
                    let run = input[s..$end].to_vec();
                    let first = run[0];
                    let c = (first.is_ascii_graphic() || first == b' ').then_some(first);
                    self.token(Kind::Key(c), run, now, &mut out);
                    if out.action.is_some() {
                        return out;
                    }
                }
            };
        }
        while i < input.len() {
            let b = input[i];
            match self.lex {
                Lex::Ground => {
                    if b == 0x1b || b == self.cfg.byte {
                        flush_plain!(i);
                        if b == 0x1b {
                            self.lex = Lex::Esc;
                            self.seq.clear();
                            self.seq.push(b);
                        } else {
                            self.token(Kind::EscPress, vec![b], now, &mut out);
                        }
                    } else if plain_start.is_none() {
                        plain_start = Some(i);
                    }
                }
                Lex::Esc => {
                    self.seq.push(b);
                    match b {
                        b'[' => self.lex = Lex::Csi,
                        b'O' => self.lex = Lex::Ss3,
                        b']' | b'P' | b'_' | b'^' | b'X' => {
                            let bytes = std::mem::take(&mut self.seq);
                            self.token(Kind::Passive, bytes, now, &mut out);
                            self.lex = Lex::Str { esc: false };
                        }
                        0x1b => {
                            // ESC ESC: the first was a key on its own.
                            self.seq.pop();
                            let bytes = std::mem::take(&mut self.seq);
                            self.token(Kind::Key(None), bytes, now, &mut out);
                            self.seq.push(b);
                        }
                        _ => {
                            let bytes = std::mem::take(&mut self.seq);
                            self.lex = Lex::Ground;
                            self.token(Kind::Key(None), bytes, now, &mut out);
                        }
                    }
                }
                Lex::Ss3 => {
                    self.seq.push(b);
                    let bytes = std::mem::take(&mut self.seq);
                    self.lex = Lex::Ground;
                    self.seq_since = None;
                    self.token(Kind::Key(None), bytes, now, &mut out);
                }
                Lex::Csi => {
                    if (0x20..=0x3f).contains(&b) && self.seq.len() < MAX_SEQ {
                        self.seq.push(b);
                    } else if (0x40..=0x7e).contains(&b) {
                        self.seq.push(b);
                        let bytes = std::mem::take(&mut self.seq);
                        self.lex = Lex::Ground;
                        self.csi(bytes, now, &mut out);
                    } else {
                        // Not a valid CSI: send what we have and re-read `b`.
                        let bytes = std::mem::take(&mut self.seq);
                        self.lex = Lex::Ground;
                        self.token(Kind::Key(None), bytes, now, &mut out);
                        self.seq_since = None;
                        if out.action.is_some() {
                            return out;
                        }
                        continue;
                    }
                }
                Lex::Mouse(left) => {
                    self.seq.push(b);
                    if left == 1 {
                        let bytes = std::mem::take(&mut self.seq);
                        self.lex = Lex::Ground;
                        self.seq_since = None;
                        self.token(Kind::Passive, bytes, now, &mut out);
                    } else {
                        self.lex = Lex::Mouse(left - 1);
                    }
                }
                Lex::Str { esc } => {
                    out.forward.push(b);
                    self.lex = match (esc, b) {
                        (_, 0x07) | (true, b'\\') => Lex::Ground,
                        (_, 0x1b) => Lex::Str { esc: true },
                        _ => Lex::Str { esc: false },
                    };
                }
                Lex::Paste(matched) => {
                    // Pasted text is user input: release anything held first.
                    self.flush_held(&mut out);
                    out.forward.push(b);
                    let m = if b == PASTE_END[matched] {
                        matched + 1
                    } else if b == PASTE_END[0] {
                        1
                    } else {
                        0
                    };
                    self.lex = if m == PASTE_END.len() {
                        Lex::Ground
                    } else {
                        Lex::Paste(m)
                    };
                }
            }
            if out.action.is_some() {
                return out;
            }
            i += 1;
        }
        flush_plain!(input.len());
        match self.lex {
            // A lone ESC at the end of a read is the Esc key: never delay it.
            Lex::Esc => {
                let bytes = std::mem::take(&mut self.seq);
                self.lex = Lex::Ground;
                self.token(Kind::Key(None), bytes, now, &mut out);
            }
            Lex::Csi | Lex::Ss3 | Lex::Mouse(_) => {
                self.seq_since.get_or_insert(now);
            }
            _ => {}
        }
        out
    }

    fn flush_held(&mut self, out: &mut Output) {
        if let State::Held { bytes, .. } | State::Command { bytes, .. } =
            std::mem::replace(&mut self.state, State::Idle)
        {
            out.forward.extend_from_slice(&bytes);
        }
    }

    fn csi(&mut self, bytes: Vec<u8>, now: u64, out: &mut Output) {
        self.seq_since = None;
        let fin = *bytes.last().unwrap();
        let body = &bytes[2..bytes.len() - 1];
        if body == b"200" && fin == b'~' {
            self.flush_held(out);
            out.forward.extend_from_slice(&bytes);
            self.lex = Lex::Paste(0);
            return;
        }
        if body.is_empty() && fin == b'M' {
            self.seq = bytes;
            self.lex = Lex::Mouse(3);
            return;
        }
        let private = body.first().is_some_and(|c| (0x3c..=0x3f).contains(c));
        let intermediate = body.iter().any(|c| (0x20..=0x2f).contains(c));
        let focus = body.is_empty() && (fin == b'I' || fin == b'O');
        if private || intermediate || focus || fin == b'R' {
            self.token(Kind::Passive, bytes, now, out);
            return;
        }
        let params = parse_params(body);
        let kind = self.classify(&params, fin);
        self.token(kind, bytes, now, out);
    }

    fn classify(&self, p: &[Vec<u32>], fin: u8) -> Kind {
        let get =
            |i: usize, j: usize, d: u32| p.get(i).and_then(|v| v.get(j)).copied().unwrap_or(d);
        let event = get(1, 1, 1);
        let ctrl_only = |m: u32| m >= 1 && ((m - 1) & !(64 | 128)) == 4;
        match fin {
            b'u' => {
                let key = get(0, 0, 0);
                let mods = get(1, 0, 1);
                if key == self.cfg.codepoint() && ctrl_only(mods) {
                    if event == 3 {
                        Kind::EscRelease
                    } else {
                        Kind::EscPress
                    }
                } else if event == 3 {
                    Kind::Release
                } else if (mods.saturating_sub(1) & !(64 | 128)) == 0 && (0x20..0x7f).contains(&key)
                {
                    Kind::Key(Some(key as u8))
                } else {
                    Kind::Key(None)
                }
            }
            b'~' if get(0, 0, 0) == 27 => {
                if get(2, 0, 0) == self.cfg.codepoint() && ctrl_only(get(1, 0, 1)) {
                    Kind::EscPress
                } else {
                    Kind::Key(None)
                }
            }
            _ if event == 3 => Kind::Release,
            _ => Kind::Key(None),
        }
    }

    fn token(&mut self, kind: Kind, bytes: Vec<u8>, now: u64, out: &mut Output) {
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = match (state, kind) {
            (State::Idle, Kind::EscPress) => State::Held {
                bytes,
                until: now + self.cfg.window_ms,
            },
            (State::Idle, _) => {
                out.forward.extend_from_slice(&bytes);
                State::Idle
            }
            (s, Kind::Passive) => {
                out.forward.extend_from_slice(&bytes);
                s
            }
            (
                State::Held {
                    bytes: mut held,
                    until,
                },
                Kind::EscRelease | Kind::Release,
            ) => {
                held.extend_from_slice(&bytes);
                State::Held { bytes: held, until }
            }
            (
                State::Held {
                    bytes: mut held, ..
                },
                Kind::EscPress,
            ) => {
                held.extend_from_slice(&bytes);
                State::Command {
                    bytes: held,
                    until: now + self.cfg.command_timeout_ms,
                }
            }
            (
                State::Command {
                    bytes: mut held,
                    until,
                },
                Kind::EscRelease | Kind::Release,
            ) => {
                held.extend_from_slice(&bytes);
                State::Command { bytes: held, until }
            }
            (State::Command { bytes: held, .. }, Kind::Key(Some(c))) => {
                match c.to_ascii_lowercase() {
                    b'd' => out.action = Some(Action::Detach),
                    b'x' => out.action = Some(Action::Exit),
                    _ => {
                        out.forward.extend_from_slice(&held);
                        out.forward.extend_from_slice(&bytes);
                    }
                }
                State::Idle
            }
            (State::Held { bytes: held, .. } | State::Command { bytes: held, .. }, _) => {
                out.forward.extend_from_slice(&held);
                out.forward.extend_from_slice(&bytes);
                State::Idle
            }
        };
    }
}

/// `1;5:3` → `[[1], [5, 3]]`; empty fields are 0.
fn parse_params(body: &[u8]) -> Vec<Vec<u32>> {
    std::str::from_utf8(body)
        .unwrap_or("")
        .split(';')
        .map(|f| f.split(':').map(|n| n.parse().unwrap_or(0)).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CB: u8 = 0x1d;

    /// Feed a script of (time, bytes) steps, ticking before each; returns all
    /// forwarded bytes and the first action.
    fn run(steps: &[(u64, &[u8])], end: u64) -> (Vec<u8>, Option<Action>) {
        let mut d = Detector::new(Config::default());
        let mut fwd = Vec::new();
        for (t, bytes) in steps {
            let o = d.feed(bytes, *t);
            fwd.extend(o.forward);
            if o.action.is_some() {
                return (fwd, o.action);
            }
        }
        let o = d.tick(end);
        fwd.extend(o.forward);
        (fwd, o.action)
    }

    #[test]
    fn plain_input_passes_straight_through() {
        let mut d = Detector::new(Config::default());
        let o = d.feed(b"ls -l\r", 0);
        assert_eq!(o.forward, b"ls -l\r");
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn lone_escape_key_is_held_then_forwarded_after_the_window() {
        let mut d = Detector::new(Config::default());
        assert!(d.feed(&[CB], 0).forward.is_empty());
        assert_eq!(d.deadline(), Some(400));
        assert!(d.tick(399).forward.is_empty());
        assert_eq!(d.tick(400).forward, vec![CB]);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn escape_then_other_key_sends_both_immediately() {
        assert_eq!(run(&[(0, &[CB]), (100, b"a")], 100), (vec![CB, b'a'], None));
        assert_eq!(run(&[(0, &[CB, b'q'])], 0), (vec![CB, b'q'], None));
    }

    #[test]
    fn double_tap_then_d_detaches_and_x_exits() {
        assert_eq!(
            run(&[(0, &[CB]), (200, &[CB]), (900, b"d")], 900),
            (vec![], Some(Action::Detach))
        );
        assert_eq!(
            run(&[(0, &[CB, CB, b'x'])], 0),
            (vec![], Some(Action::Exit))
        );
        assert_eq!(
            run(&[(0, &[CB, CB, b'D'])], 0),
            (vec![], Some(Action::Detach))
        );
    }

    #[test]
    fn armed_from_the_double_tap_until_the_key_or_the_timeout() {
        let mut d = Detector::new(Config::default());
        d.feed(&[CB], 0);
        assert!(!d.armed(), "one press is not command mode");
        d.feed(&[CB], 100);
        assert!(d.armed());
        d.tick(2100);
        assert!(!d.armed(), "timed out");

        let mut d = Detector::new(Config::default());
        d.feed(&[CB, CB], 0);
        assert!(d.armed());
        d.feed(b"q", 10);
        assert!(!d.armed(), "an unknown key ends it");

        // Never inside a paste.
        let mut d = Detector::new(Config::default());
        d.feed(b"\x1b[200~\x1d\x1d", 0);
        assert!(!d.armed());
    }

    #[test]
    fn slow_double_tap_is_two_literal_presses() {
        let (fwd, act) = run(&[(0, &[CB]), (401, &[CB]), (500, b"d")], 1000);
        assert_eq!(act, None);
        assert_eq!(fwd, vec![CB, CB, b'd']);
    }

    #[test]
    fn unknown_command_key_forwards_everything_as_typed() {
        assert_eq!(run(&[(0, &[CB, CB, b'q'])], 0), (vec![CB, CB, b'q'], None));
        // A third escape press is "any other key".
        assert_eq!(run(&[(0, &[CB, CB, CB])], 0), (vec![CB, CB, CB], None));
    }

    #[test]
    fn command_mode_times_out_after_two_seconds() {
        let mut d = Detector::new(Config::default());
        d.feed(&[CB, CB], 0);
        assert_eq!(d.deadline(), Some(2000));
        assert!(d.tick(1999).forward.is_empty());
        assert_eq!(d.tick(2000).forward, vec![CB, CB]);
    }

    #[test]
    fn kitty_encoding_press_and_release() {
        let press = b"\x1b[93;5u";
        let (fwd, act) = run(&[(0, press), (100, press), (150, b"d")], 150);
        assert_eq!((fwd, act), (vec![], Some(Action::Detach)));

        // With event types: press, release, press, release, then `x` press.
        let seq: &[u8] = b"\x1b[93;5:1u\x1b[93;5:3u\x1b[93;5:1u\x1b[93;5:3u\x1b[120;1:1u";
        assert_eq!(run(&[(0, seq)], 0), (vec![], Some(Action::Exit)));

        // Lock modifiers (caps lock = 64) do not hide the key.
        assert_eq!(
            run(&[(0, b"\x1b[93;69u\x1b[93;69ud")], 0),
            (vec![], Some(Action::Detach))
        );
    }

    #[test]
    fn kitty_release_follows_its_press_when_forwarded() {
        let seq: &[u8] = b"\x1b[93;5:1u\x1b[93;5:3u";
        let (fwd, act) = run(&[(0, seq), (10, b"a")], 10);
        assert_eq!(act, None);
        assert_eq!(fwd, [seq, b"a"].concat());
        // Timeout forwards press and release together.
        assert_eq!(run(&[(0, seq)], 1000), (seq.to_vec(), None));
    }

    #[test]
    fn modify_other_keys_encoding() {
        let p: &[u8] = b"\x1b[27;5;93~";
        assert_eq!(
            run(&[(0, p), (50, p), (60, b"d")], 60),
            (vec![], Some(Action::Detach))
        );
    }

    #[test]
    fn other_ctrl_modified_keys_are_not_the_escape() {
        // Ctrl+Shift+] and Alt+] in kitty are different keys.
        let seq: &[u8] = b"\x1b[93;6u\x1b[93;6u";
        assert_eq!(run(&[(0, seq)], 0), (seq.to_vec(), None));
        let seq: &[u8] = b"\x1b[27;3;93~\x1b[27;3;93~d";
        assert_eq!(run(&[(0, seq)], 0), (seq.to_vec(), None));
    }

    #[test]
    fn escape_byte_inside_a_sequence_never_matches() {
        // An OSC reply carrying 0x1d, a DCS reply, then a real keypress.
        let seq: &[u8] = b"\x1b]11;rgb:\x1d\x1d\x07\x1bP>|kitty\x1d\x1b\\a";
        assert_eq!(run(&[(0, seq)], 0), (seq.to_vec(), None));
    }

    #[test]
    fn sequences_split_across_reads_are_reassembled() {
        let whole: &[u8] = b"\x1b[93;5u\x1b[93;5ud";
        for split in 1..whole.len() {
            // A read ending in a lone ESC is the Esc key by design (never
            // delayed), so a split right after an ESC is not reassembled.
            if whole[split - 1] == 0x1b {
                continue;
            }
            let (fwd, act) = run(&[(0, &whole[..split]), (1, &whole[split..])], 1);
            assert_eq!(
                (fwd, act),
                (vec![], Some(Action::Detach)),
                "split at {split}"
            );
        }
    }

    #[test]
    fn incomplete_sequence_is_released_after_a_short_wait() {
        let mut d = Detector::new(Config::default());
        assert!(d.feed(b"\x1b[1;5", 0).forward.is_empty());
        assert_eq!(d.deadline(), Some(PENDING_MS));
        assert_eq!(d.tick(PENDING_MS).forward, b"\x1b[1;5");
    }

    #[test]
    fn lone_esc_at_end_of_read_is_not_delayed() {
        let mut d = Detector::new(Config::default());
        assert_eq!(d.feed(b"\x1b", 0).forward, b"\x1b");
        assert_eq!(d.deadline(), None);
        assert_eq!(d.feed(b"\x1b\x1b", 0).forward, b"\x1b\x1b");
    }

    #[test]
    fn bracketed_paste_is_never_a_command() {
        let paste: &[u8] = b"\x1b[200~\x1d\x1dd\x1b[93;5u\x1b[201~";
        assert_eq!(run(&[(0, paste)], 0), (paste.to_vec(), None));
        // Split inside the paste and inside both markers. (Split 1 leaves a
        // lone ESC at the end of a read, which is the Esc key by design;
        // terminals write a paste's start marker in one write.)
        for split in 2..paste.len() {
            let (fwd, act) = run(&[(0, &paste[..split]), (1, &paste[split..])], 1);
            assert_eq!((fwd, act), (paste.to_vec(), None), "split at {split}");
        }
        // After the paste ends, detection works again.
        let (fwd, act) = run(&[(0, paste), (1, &[CB, CB, b'd'])], 1);
        assert_eq!((fwd, act), (paste.to_vec(), Some(Action::Detach)));
    }

    #[test]
    fn paste_releases_a_held_escape_first() {
        let paste: &[u8] = b"\x1b[200~hi\x1b[201~";
        assert_eq!(
            run(&[(0, &[CB]), (10, paste)], 10),
            ([&[CB][..], paste].concat(), None)
        );
    }

    #[test]
    fn replies_and_mouse_do_not_break_a_double_tap_and_are_not_delayed() {
        let mut d = Detector::new(Config::default());
        assert!(d.feed(&[CB], 0).forward.is_empty());
        // A DA reply, an SGR mouse move, a legacy mouse report and a focus event.
        let passive: &[u8] = b"\x1b[?62;22c\x1b[<35;10;5M\x1b[M !!\x1b[I";
        assert_eq!(d.feed(passive, 50).forward, passive);
        let o = d.feed(&[CB, b'd'], 100);
        assert_eq!(o.action, Some(Action::Detach));
    }

    #[test]
    fn detach_ignores_the_rest_of_the_read() {
        let mut d = Detector::new(Config::default());
        let o = d.feed(b"ab\x1d\x1ddrm -rf", 0);
        assert_eq!(o.forward, b"ab");
        assert_eq!(o.action, Some(Action::Detach));
    }

    #[test]
    fn configurable_key_and_window() {
        let cfg = Config {
            byte: Config::parse_key("^A").unwrap(),
            window_ms: 100,
            command_timeout_ms: 2000,
        };
        assert_eq!(cfg.byte, 0x01);
        let mut d = Detector::new(cfg);
        assert_eq!(d.feed(&[1, 1, b'd'], 0).action, Some(Action::Detach));
        // Kitty form of Ctrl+A is key 97.
        let mut d = Detector::new(cfg);
        assert_eq!(
            d.feed(b"\x1b[97;5u\x1b[97;5ux", 0).action,
            Some(Action::Exit)
        );
        // Ctrl-] is now an ordinary key.
        let mut d = Detector::new(cfg);
        assert_eq!(d.feed(&[CB, CB, b'd'], 0).forward, vec![CB, CB, b'd']);
        assert_eq!(Config::parse_key("^]"), Some(0x1d));
        assert_eq!(Config::parse_key("^\\"), Some(0x1c));
        assert_eq!(Config::parse_key("x"), None);
        assert_eq!(Config::parse_key("^["), None);
    }
}
