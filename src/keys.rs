//! Command mode: Ctrl-] Ctrl-] followed by a command key (DESIGN §6).
//!
//! [`Detector`] is a pure state machine over stdin bytes and an injected
//! millisecond clock. It forwards everything unchanged except the escape
//! presses it consumes, recognising the escape key in all three encodings a
//! terminal may use (legacy byte, kitty `CSI … u`, xterm modifyOtherKeys),
//! never matching inside another escape sequence, and passing bracketed pastes
//! through untouched. [`PasteTracker`] follows the pastes in what was sent.

/// What a completed command asks the client to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the session running and exit the client.
    Detach,
    /// End the session on the remote and exit.
    Exit,
}

/// The shortest the window to choose the command key ever gets — the old
/// bare constant, now a floor rather than the whole answer (acs-mq0).
const COMMAND_FLOOR_MS: u64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The legacy control byte of the escape key (Ctrl-] = 0x1D).
    pub byte: u8,
    /// The escape window (`ACS_ESCAPE_TIMEOUT_MS`): the max gap between the
    /// two escape presses, and — through `command_window_ms` — the window
    /// to then choose the command key.
    pub window_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            byte: 0x1d,
            window_ms: 400,
        }
    }
}

impl Config {
    /// How long command mode waits for the command key: the configured
    /// escape window, never less than 2 s (DESIGN §6.1).
    ///
    /// One setting covers both halves of the gesture, because someone who
    /// widens the escape window has said they are slower than the default
    /// and means the choice too (acs-mq0). It is a floor and not the value
    /// itself so that *tightening* the double tap — which says nothing
    /// about how fast the user can pick a key — never takes the 2 s to
    /// choose away with it.
    fn command_window_ms(&self) -> u64 {
        self.window_ms.max(COMMAND_FLOOR_MS)
    }

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

/// Longest string (OSC, DCS, APC) the detector will stay inside. The
/// replies acs's own sequences draw are a handful of bytes; a clipboard
/// read can be larger, so this is generous, but it is not unbounded
/// (acs-55v).
const MAX_STR: usize = 64 * 1024;
const MAX_SEQ: usize = 64;
const PASTE_START: &[u8] = b"\x1b[200~";
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
    /// Inside a terminal reply's string — OSC, DCS or APC — until BEL or
    /// ST, or [`PENDING_MS`] without a byte; `esc` = saw ESC.
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
    /// When the last byte of a string (`Lex::Str`) arrived.
    str_last: u64,
    /// Bytes taken by the string being lexed, held to [`MAX_STR`] so a
    /// remote cannot keep the detector inside one (acs-55v).
    str_bytes: usize,
    state: State,
}

impl Detector {
    pub fn new(cfg: Config) -> Self {
        Detector {
            cfg,
            lex: Lex::Ground,
            seq: Vec::new(),
            seq_since: None,
            str_last: 0,
            str_bytes: 0,
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
        // A reply's string arrives in a burst; a pause means `ESC ]`, `ESC P`
        // or `ESC _` was a Meta key (Alt+_ in readline), not a reply, so what
        // follows is keys again (acs-evv). Its bytes went out as they came,
        // so nothing is due here and no deadline is needed for it.
        if matches!(self.lex, Lex::Str { .. }) && now >= self.str_last + PENDING_MS {
            self.lex = Lex::Ground;
        }
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
                        // The strings terminals reply with: OSC (colours,
                        // clipboard), DCS (DECRQSS, XTGETTCAP), APC (kitty
                        // graphics). PM and SOS carry none: `ESC ^` and
                        // `ESC X` are Meta keys.
                        b']' | b'P' | b'_' => {
                            let bytes = std::mem::take(&mut self.seq);
                            self.token(Kind::Passive, bytes, now, &mut out);
                            self.lex = Lex::Str { esc: false };
                            self.str_last = now;
                            self.str_bytes = 0;
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
                    self.str_last = now;
                    self.str_bytes += 1;
                    self.lex = match (esc, b) {
                        (_, 0x07) | (true, b'\\') => Lex::Ground,
                        // Long enough to be no terminal reply acs asked
                        // for. The idle timeout above frees a Meta key
                        // that was mistaken for a string, but only once
                        // the bytes stop; a remote that keeps them coming
                        // would otherwise hold the detector here for as
                        // long as it liked, and Ctrl-] Ctrl-] d would be
                        // forwarded to it instead of detaching (acs-55v).
                        _ if self.str_bytes >= MAX_STR => Lex::Ground,
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
                    until: now + self.cfg.command_window_ms(),
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

/// Whether the input sent to the program so far ends inside a bracketed
/// paste (`CSI 200 ~` without its `CSI 201 ~` yet), so that a byte the client
/// adds of its own — the Ctrl-L after a reconnect (DESIGN §5.2) — never lands
/// in the middle of pasted text. Markers split across writes are found.
#[derive(Debug, Default)]
pub struct PasteTracker {
    open: bool,
    /// The last bytes seen, which may be the start of a marker.
    tail: Vec<u8>,
}

impl PasteTracker {
    /// Bytes just sent to the program.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(bytes);
        for (i, _) in buf.iter().enumerate().filter(|(_, b)| **b == 0x1b) {
            if buf[i..].starts_with(PASTE_START) {
                self.open = true;
            } else if buf[i..].starts_with(PASTE_END) {
                self.open = false;
            }
        }
        // Shorter than a marker, so a whole one is never counted twice.
        let keep = buf.len().min(PASTE_END.len() - 1);
        self.tail = buf.split_off(buf.len() - keep);
    }

    pub fn open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paste_tracker_follows_markers_split_anywhere() {
        let input: &[u8] = b"ls\x1b[200~pasted\x1b[201~\x1b[200~more";
        let open_after = |sent: &[u8]| {
            let mut p = PasteTracker::default();
            p.feed(sent);
            p.open()
        };
        assert!(!open_after(b"ls\x0c\x1b[A"));
        assert!(open_after(input));
        assert!(!open_after(&input[..input.len() - 10]), "closed again");
        assert!(!open_after(b"\x1b[200"), "half a marker opens nothing");
        for split in 0..=input.len() {
            let mut p = PasteTracker::default();
            p.feed(&input[..split]);
            p.feed(&input[split..]);
            assert!(p.open(), "split at {split}");
        }
        // One byte at a time, and the end marker after it.
        let mut p = PasteTracker::default();
        for b in input.iter().chain(b"\x1b[201~") {
            p.feed(std::slice::from_ref(b));
        }
        assert!(!p.open());
    }

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

    /// Regression (acs-mq0): the configured escape window is the window to
    /// choose the command key too, not only the gap between the two
    /// presses. It used to be a bare 2 s constant, so widening the setting
    /// because 400 ms was too quick relaxed one half of the gesture and
    /// left the other exactly as it was.
    #[test]
    fn the_escape_window_is_also_the_window_to_choose_the_command_key() {
        let cfg = Config {
            window_ms: 30_000,
            ..Config::default()
        };
        let mut d = Detector::new(cfg);
        d.feed(&[CB], 0);
        d.feed(&[CB], 20_000);
        assert!(d.armed());
        assert_eq!(d.deadline(), Some(50_000));
        // Long past the old 2 s, well inside the configured window.
        assert!(d.tick(49_999).forward.is_empty());
        assert_eq!(d.feed(b"d", 49_999).action, Some(Action::Detach));

        // And it still ends: the configured window, not forever.
        let mut d = Detector::new(cfg);
        d.feed(&[CB, CB], 0);
        assert_eq!(d.tick(30_000).forward, vec![CB, CB]);
        assert!(!d.armed());
    }

    /// The other direction of acs-mq0: a *tighter* escape window says the
    /// user wants the double tap crisp, not that they pick a command key in
    /// under 400 ms — so 2 s is a floor, and the default is untouched.
    #[test]
    fn a_tighter_escape_window_keeps_the_two_seconds_to_choose() {
        let mut d = Detector::new(Config {
            window_ms: 100,
            ..Config::default()
        });
        d.feed(&[CB, CB], 0);
        assert_eq!(d.deadline(), Some(2000));
        assert_eq!(d.feed(b"d", 1999).action, Some(Action::Detach));

        // Unset, both windows are what they always were.
        let cfg = Config::default();
        assert_eq!(cfg.window_ms, 400);
        assert_eq!(cfg.command_window_ms(), 2000);
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

    /// Regression (acs-evv): Alt+_ (readline's yank-last-arg) sends `ESC _`,
    /// which is also how an APC reply starts. It must not leave the escape
    /// key unheard until a BEL comes.
    #[test]
    fn a_meta_key_that_looks_like_a_string_start_does_not_eat_the_escape() {
        for meta in [b"\x1b_", b"\x1b]", b"\x1bP", b"\x1b^", b"\x1bX"] {
            let (fwd, action) = run(
                &[(0, meta), (5000, &[CB]), (5050, &[CB]), (5100, b"d")],
                5100,
            );
            assert_eq!(action, Some(Action::Detach), "after {meta:?}");
            assert_eq!(fwd, meta.to_vec(), "after {meta:?}");
        }
        // A key right after it, then the command a normal while later.
        let (_, action) = run(
            &[
                (0, b"\x1b_"),
                (40, b"x"),
                (1000, &[CB]),
                (1050, &[CB]),
                (1100, b"d"),
            ],
            1100,
        );
        assert_eq!(action, Some(Action::Detach));
    }

    /// acs-55v: the idle timeout frees a Meta key that was taken for a
    /// string, but only once the bytes stop. A remote that keeps them
    /// coming — a huge clipboard set with OSC 52 and read back in a loop —
    /// would otherwise hold the detector inside the string for as long as
    /// it liked, so Ctrl-] Ctrl-] d went to it instead of detaching, and
    /// the session could not be left without killing the terminal.
    #[test]
    fn an_unending_string_cannot_swallow_the_command_key() {
        let mut d = Detector::new(Config::default());
        // An OSC that never terminates, arriving without a pause.
        let mut t = 0;
        d.feed(b"\x1b]52;c;", t);
        for _ in 0..(MAX_STR / 1024 + 2) {
            t += 1;
            d.feed(&vec![b'A'; 1024], t);
        }
        // The command key is heard again, with no gap in the stream.
        t += 1;
        let o = d.feed(&[CB], t);
        assert!(o.forward.is_empty(), "the escape was forwarded: {o:?}");
        t += 1;
        d.feed(&[CB], t);
        t += 1;
        let o = d.feed(b"d", t);
        assert_eq!(o.action, Some(Action::Detach));
    }

    /// acs-55v: a reply of an ordinary size is still read as one, so the
    /// cap does not break what the string state is for.
    #[test]
    fn an_ordinary_reply_is_still_taken_as_a_string() {
        let mut d = Detector::new(Config::default());
        let reply = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07";
        let o = d.feed(reply, 0);
        assert_eq!(o.forward, reply.to_vec());
        assert_eq!(o.action, None);
        // And the command key works right after it, with no pause.
        d.feed(&[CB], 1);
        d.feed(&[CB], 2);
        assert_eq!(d.feed(b"d", 3).action, Some(Action::Detach));
    }

    /// PM and SOS are not strings terminals reply with: `ESC ^` / `ESC X`
    /// are keys, so a command right after them works (acs-evv).
    #[test]
    fn pm_and_sos_introducers_are_keys() {
        for meta in [b"\x1b^", b"\x1bX"] {
            let mut input = meta.to_vec();
            input.extend_from_slice(&[CB, CB, b'd']);
            assert_eq!(
                run(&[(0, &input)], 0),
                (meta.to_vec(), Some(Action::Detach))
            );
        }
    }

    /// A reply split across reads a little apart is still one string: the
    /// escape byte inside it is not a key press.
    #[test]
    fn a_reply_split_across_reads_stays_a_string() {
        let (fwd, action) = run(
            &[
                (0, b"\x1b]52;c;"),
                (30, &[CB, CB]),
                (60, b"d\x07"),
                (70, b"z"),
            ],
            70,
        );
        assert_eq!(action, None);
        assert_eq!(fwd, b"\x1b]52;c;\x1d\x1dd\x07z");
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
