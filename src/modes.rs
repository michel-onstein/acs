//! Passive observer of terminal modes (DESIGN §6.4).
//!
//! Scans the output stream for the few sequences that switch terminal modes a
//! remote program should not leave behind — mouse reporting, bracketed paste,
//! the alternate screen, a hidden cursor, keyboard protocols, synchronized
//! output — and produces the resets for when the client leaves without the
//! program having cleaned up (detach, exit, abandoned reconnect).
//!
//! It never modifies, delays or reorders the stream: the caller writes the
//! bytes to the terminal and hands the same bytes to [`ModeObserver::observe`].
//! It also knows where the stream is between sequences and characters
//! ([`ModeObserver::at_boundary`]), the only place the client may put a byte
//! of its own, the command-mode bell (DESIGN §6.1) — and, for a write that
//! cannot wait for one, how to step out of the sequence the stream is in and
//! back into it ([`ModeObserver::interrupt_sequence`],
//! [`ModeObserver::reopen_sequence`], DESIGN §7, acs-p4u).

use std::collections::BTreeMap;

/// DEC private modes worth resetting, with their power-on default.
const TRACKED: &[(u16, bool)] = &[
    (1, false),    // application cursor keys
    (25, true),    // cursor visible
    (47, false),   // alternate screen (old)
    (1047, false), // alternate screen
    (1049, false), // alternate screen + saved cursor
    (1000, false), // mouse: press/release
    (1002, false), // mouse: button motion
    (1003, false), // mouse: any motion
    (1004, false), // focus events
    (1005, false), // mouse: UTF-8 coordinates
    (1006, false), // mouse: SGR coordinates
    (1015, false), // mouse: urxvt coordinates
    (1016, false), // mouse: SGR pixel coordinates
    (2004, false), // bracketed paste
    (2026, false), // synchronized output
];

const ALT_SCREEN: &[u16] = &[1049, 1047, 47];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Ground,
    Esc,
    Csi,
    /// OSC/DCS/APC/PM/SOS body; `esc` = previous byte was ESC.
    Str {
        esc: bool,
    },
}

#[derive(Default)]
pub struct ModeObserver {
    lex: Option<Lex>,
    seq: Vec<u8>,
    /// The CSI now being read was longer than `seq` keeps, so its bytes
    /// are no longer all here to write again ([`ModeObserver::reopen_sequence`]).
    seq_full: bool,
    /// DEC modes currently different from their default: mode → value.
    dec: BTreeMap<u16, bool>,
    /// Kitty keyboard flags pushed with `CSI > flags u` and not popped.
    kitty_depth: u32,
    /// Kitty flags set in place with `CSI = flags ; mode u`.
    kitty_set: bool,
    /// xterm modifyOtherKeys level, if non-zero.
    modify_other_keys: bool,
    /// Keypad application mode (`ESC =`).
    keypad_app: bool,
    /// A scroll region was set.
    scroll_region: bool,
    /// Continuation bytes still due for a UTF-8 character.
    utf8_left: u8,
    /// The bytes of that character seen so far.
    utf8: Vec<u8>,
}

impl ModeObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything, e.g. after a fresh attach where the local terminal
    /// was cleared.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Forget where the stream was but keep the modes, after a resume that
    /// skipped output (a gap): the program is the same and still in its
    /// modes, but the next byte may start anywhere (acs-xk4).
    pub fn resync(&mut self) {
        self.lex = None;
        self.seq.clear();
        self.seq_full = false;
        self.utf8_left = 0;
        self.utf8.clear();
    }

    /// True when the terminal is believed to be in its default state.
    pub fn is_clean(&self) -> bool {
        self.dec.is_empty()
            && self.kitty_depth == 0
            && !self.kitty_set
            && !self.modify_other_keys
            && !self.keypad_app
            && !self.scroll_region
    }

    pub fn observe(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    /// True between sequences and characters: a byte written here (a BEL)
    /// cannot end an OSC/DCS string early or split a UTF-8 character.
    pub fn at_boundary(&self) -> bool {
        self.lex.unwrap_or(Lex::Ground) == Lex::Ground && self.utf8_left == 0
    }

    /// Observe `bytes` up to the first boundary ([`ModeObserver::at_boundary`]);
    /// returns how many were observed to get there (0 if already at one), or
    /// `None` when all of them were and none came.
    pub fn observe_to_boundary(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut i = 0;
        loop {
            if self.at_boundary() {
                return Some(i);
            }
            self.step(*bytes.get(i)?);
            i += 1;
        }
    }

    /// Bytes that end whatever the stream has left unfinished, for a write
    /// of acs's own that cannot wait for a boundary — the status line, the
    /// resets on the way out, the clear a reattach writes (DESIGN §7,
    /// acs-p4u). Empty at a boundary.
    ///
    /// `ESC \` (ST) is the one answer that works for every state: it is the
    /// defined terminator of an OSC, DCS, APC, PM or SOS string, and its ESC
    /// alone cancels a half-written CSI or character, since ESC starts a new
    /// sequence from any state. Written at a boundary it would be an ST with
    /// no string open, which terminals ignore — but it is not written there.
    pub fn interrupt_sequence(&self) -> &'static [u8] {
        match self.at_boundary() {
            true => b"",
            false => b"\x1b\\",
        }
    }

    /// Bytes that put the terminal back inside the sequence
    /// [`ModeObserver::interrupt_sequence`] ended, for a caller whose write
    /// is only passing through and whose stream carries on afterwards: the
    /// program's next byte then means what the program meant by it.
    ///
    /// `None` where acs cannot do that faithfully, and the rest of the
    /// sequence will be read as text:
    ///
    /// - an OSC/DCS/APC/PM/SOS string, whose body the terminal may already
    ///   have acted on (a DCS is passed through as it arrives, and ending
    ///   one dispatches what there is of it) — writing it again would do it
    ///   twice, which is worse than the junk;
    /// - a CSI longer than the 64 parameter bytes kept here, whose bytes are
    ///   no longer all known.
    pub fn reopen_sequence(&self) -> Option<Vec<u8>> {
        match self.lex.unwrap_or(Lex::Ground) {
            // Nothing open, or a character half read: its bytes have done
            // nothing yet, so writing them again costs only the replacement
            // character the interrupted one leaves behind.
            Lex::Ground => match self.utf8_left {
                0 => Some(Vec::new()),
                _ => Some(self.utf8.clone()),
            },
            Lex::Esc => Some(b"\x1b".to_vec()),
            Lex::Csi if !self.seq_full => {
                let mut out = b"\x1b[".to_vec();
                out.extend_from_slice(&self.seq);
                Some(out)
            }
            _ => None,
        }
    }

    /// An ESC starts a sequence: forget the one it interrupted, if any.
    fn start_seq(&mut self) {
        self.seq.clear();
        self.seq_full = false;
    }

    fn step(&mut self, b: u8) {
        let lex = self.lex.unwrap_or(Lex::Ground);
        self.lex = Some(match lex {
            Lex::Ground => {
                match b {
                    0x80..=0xbf => {
                        if self.utf8_left > 0 {
                            self.utf8.push(b);
                        }
                        self.utf8_left = self.utf8_left.saturating_sub(1);
                    }
                    0xc2..=0xf4 => {
                        self.utf8_left = match b {
                            0xc2..=0xdf => 1,
                            0xe0..=0xef => 2,
                            _ => 3,
                        };
                        self.utf8.clear();
                        self.utf8.push(b);
                    }
                    _ => self.utf8_left = 0,
                }
                if b == 0x1b {
                    self.start_seq();
                    Lex::Esc
                } else {
                    Lex::Ground
                }
            }
            Lex::Esc => match b {
                b'[' => Lex::Csi,
                b']' | b'P' | b'_' | b'^' | b'X' => Lex::Str { esc: false },
                b'=' => {
                    self.keypad_app = true;
                    Lex::Ground
                }
                b'>' => {
                    self.keypad_app = false;
                    Lex::Ground
                }
                b'c' => {
                    // RIS: full reset.
                    self.clear();
                    Lex::Ground
                }
                0x1b => Lex::Esc,
                _ => Lex::Ground,
            },
            Lex::Csi => {
                if (0x20..=0x3f).contains(&b) {
                    if self.seq.len() < 64 {
                        self.seq.push(b);
                    } else {
                        self.seq_full = true;
                    }
                    Lex::Csi
                } else if (0x40..=0x7e).contains(&b) {
                    let body = std::mem::take(&mut self.seq);
                    self.seq_full = false;
                    self.csi(&body, b);
                    Lex::Ground
                } else if b == 0x1b {
                    self.start_seq();
                    Lex::Esc
                } else {
                    // C0 controls inside CSI are executed; keep lexing.
                    Lex::Csi
                }
            }
            Lex::Str { esc } => match (esc, b) {
                (_, 0x07) | (true, b'\\') => Lex::Ground,
                (_, 0x1b) => Lex::Str { esc: true },
                _ => Lex::Str { esc: false },
            },
        });
    }

    fn csi(&mut self, body: &[u8], fin: u8) {
        let Ok(body) = std::str::from_utf8(body) else {
            return;
        };
        let nums = |s: &str| -> Vec<u32> { s.split(';').map(|n| n.parse().unwrap_or(0)).collect() };
        match (body.as_bytes().first(), fin) {
            (Some(b'?'), b'h' | b'l') => {
                let on = fin == b'h';
                for m in nums(&body[1..]) {
                    let Ok(m) = u16::try_from(m) else { continue };
                    if let Some(&(_, default)) = TRACKED.iter().find(|(t, _)| *t == m) {
                        if on == default {
                            self.dec.remove(&m);
                        } else {
                            self.dec.insert(m, on);
                        }
                    }
                }
            }
            (Some(b'>'), b'u') => self.kitty_depth += 1,
            (Some(b'<'), b'u') => {
                let n = body[1..].parse().unwrap_or(1).max(1);
                self.kitty_depth = self.kitty_depth.saturating_sub(n);
            }
            (Some(b'='), b'u') => {
                let p = nums(&body[1..]);
                if self.kitty_depth == 0 {
                    self.kitty_set = p.first().copied().unwrap_or(0) != 0;
                }
            }
            (Some(b'>'), b'm') => {
                let p = nums(&body[1..]);
                if p.first() == Some(&4) {
                    self.modify_other_keys = p.get(1).copied().unwrap_or(0) != 0;
                }
            }
            (None | Some(b'0'..=b'9' | b';'), b'r') => {
                let p = nums(body);
                self.scroll_region = !(body.is_empty() || p.iter().all(|&n| n == 0));
            }
            _ => {}
        }
    }

    /// Bytes that return the terminal to its defaults for every mode seen.
    /// Empty when nothing needs resetting.
    pub fn reset_sequence(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.is_clean() {
            return out;
        }
        if self.kitty_depth > 0 {
            out.extend_from_slice(format!("\x1b[<{}u", self.kitty_depth).as_bytes());
        }
        if self.kitty_set {
            out.extend_from_slice(b"\x1b[=0;1u");
        }
        if self.modify_other_keys {
            out.extend_from_slice(b"\x1b[>4;0m");
        }
        if self.keypad_app {
            out.extend_from_slice(b"\x1b>");
        }
        if self.scroll_region {
            out.extend_from_slice(b"\x1b[r");
        }
        let restore = |out: &mut Vec<u8>, m: u16| {
            let default = TRACKED
                .iter()
                .find(|(t, _)| *t == m)
                .map(|t| t.1)
                .unwrap_or(false);
            out.extend_from_slice(
                format!("\x1b[?{}{}", m, if default { 'h' } else { 'l' }).as_bytes(),
            );
        };
        for &m in self.dec.keys() {
            if !ALT_SCREEN.contains(&m) && m != 25 {
                restore(&mut out, m);
            }
        }
        // Leave the alternate screen last but one, so the resets above apply
        // to the screen the user returns to, then show the cursor.
        for &m in ALT_SCREEN {
            if self.dec.contains_key(&m) {
                restore(&mut out, m);
            }
        }
        if self.dec.contains_key(&25) {
            restore(&mut out, 25);
        }
        out.extend_from_slice(b"\x1b[0m");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe_split(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut resets = Vec::new();
        for split in 0..=stream.len() {
            let mut o = ModeObserver::new();
            o.observe(&stream[..split]);
            o.observe(&stream[split..]);
            resets.push(o.reset_sequence());
        }
        resets
    }

    #[test]
    fn clean_stream_needs_no_reset() {
        let mut o = ModeObserver::new();
        o.observe(b"hello \x1b[1;31mred\x1b[0m \x1b]0;title\x07 \x1b[?1049h\x1b[?1049l");
        assert!(o.is_clean());
        assert!(o.reset_sequence().is_empty());
    }

    #[test]
    fn full_screen_tui_is_reset_in_order() {
        // What a vim/htop-like program turns on.
        let stream: &[u8] =
            b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?25l\x1b[?1000;1002;1006h\x1b[?2004h\x1b[>1u\x1b[>4;2m\x1b[?1004h\x1b[5;20r";
        for (split, reset) in observe_split(stream).into_iter().enumerate() {
            assert_eq!(
                reset,
                b"\x1b[<1u\x1b[>4;0m\x1b>\x1b[r\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1004l\x1b[?1006l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m",
                "split at {split}"
            );
        }
    }

    #[test]
    fn modes_turned_off_again_are_forgotten() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b[?1000h\x1b[?25l\x1b[>1u\x1b[>1u\x1b[<u\x1b[?25h\x1b[?1000l");
        assert_eq!(o.reset_sequence(), b"\x1b[<1u\x1b[0m");
        o.observe(b"\x1b[<5u");
        assert!(o.is_clean());
    }

    #[test]
    fn sequences_inside_strings_are_ignored() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b]2;\x1b[?1000h\x07\x1bP\x1b[?2004h\x1b\\");
        // The OSC ends at BEL; the DCS body swallows its sequence until ST.
        assert!(
            o.is_clean(),
            "{:?}",
            String::from_utf8_lossy(&o.reset_sequence())
        );
    }

    #[test]
    fn boundaries_are_outside_sequences_strings_and_characters() {
        let at = |bytes: &[u8]| {
            let mut o = ModeObserver::new();
            o.observe(bytes);
            o.at_boundary()
        };
        assert!(at(b""));
        assert!(at(
            b"text \x1b[1m\x1b]0;t\x07\x1bP1$r\x1b\\\x1b=caf\xc3\xa9"
        ));
        for inside in [
            &b"\x1b"[..],
            b"\x1b[1;3",
            b"\x1b]0;title",
            b"\x1b]0;title\x1b",
            b"\x1bPq#0",
            b"\x1b_Gf=100",
            b"caf\xc3",
            b"\xe2\x82",
            b"\xf0\x9f\x98",
        ] {
            assert!(!at(inside), "{inside:?}");
        }
        // An ESC inside a UTF-8 character ends it.
        assert!(at(b"\xe2\x1b[m"));
    }

    #[test]
    fn observe_to_boundary_stops_where_the_string_ends() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b]2;ti");
        // The rest of the title, its ST, then more output and another OSC.
        let rest: &[u8] = b"tle\x1b\\after\x1b]2;next";
        assert_eq!(o.observe_to_boundary(rest), Some(5));
        assert!(o.at_boundary());
        o.observe(&rest[5..]);
        assert!(!o.at_boundary());
        // Already at one: nothing to observe.
        let mut o = ModeObserver::new();
        assert_eq!(o.observe_to_boundary(b"abc"), Some(0));
        // None comes: everything was observed.
        let mut o = ModeObserver::new();
        o.observe(b"\x1b]0;");
        assert_eq!(o.observe_to_boundary(b"a"), None);
        assert_eq!(o.observe_to_boundary(b"b\x07"), Some(2));
        // A BEL ends an OSC as ST does.
        let mut o = ModeObserver::new();
        o.observe(b"\x1b]0;x");
        assert_eq!(o.observe_to_boundary(b"\x07z"), Some(1));
    }

    /// Regression (acs-xk4): after a gap the modes stay, for leave() to
    /// reset, but a sequence cut off by the gap is forgotten.
    #[test]
    fn resync_keeps_modes_and_drops_a_half_seen_sequence() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b[?1049h\x1b[?1000h\x1b]0;half a tit");
        assert!(!o.at_boundary());
        o.resync();
        assert!(o.at_boundary());
        assert_eq!(o.reset_sequence(), b"\x1b[?1000l\x1b[?1049l\x1b[0m");
        // What follows is read from the ground, not as the old title.
        o.observe(b"\x1b[?2004h");
        assert!(o.reset_sequence().starts_with(b"\x1b[?1000l\x1b[?2004l"));
    }

    /// acs-p4u: what acs writes around a write of its own that cannot wait
    /// for a boundary. At a boundary there is nothing to write; inside a
    /// sequence, an ST ends it, and it is re-opened byte for byte where the
    /// terminal cannot have acted on it yet.
    #[test]
    fn a_sequence_is_ended_and_given_back() {
        let around = |bytes: &[u8]| {
            let mut o = ModeObserver::new();
            o.observe(bytes);
            (o.interrupt_sequence().to_vec(), o.reopen_sequence())
        };
        let ended = |give_back: &[u8]| (b"\x1b\\".to_vec(), Some(give_back.to_vec()));
        let open = (Vec::new(), Some(Vec::new()));
        // Nothing open: nothing written, either side.
        assert_eq!(around(b"plain \x1b[1;31m text"), open);
        // A CSI: its parameters and intermediates, in order, with the C0
        // controls the terminal has already executed left out.
        assert_eq!(around(b"\x1b[1;31"), ended(b"\x1b[1;31"));
        assert_eq!(around(b"\x1b[?1049"), ended(b"\x1b[?1049"));
        assert_eq!(around(b"\x1b[1;\r31"), ended(b"\x1b[1;31"));
        // A lone ESC, and an ESC that ended a CSI of its own.
        assert_eq!(around(b"\x1b"), ended(b"\x1b"));
        assert_eq!(around(b"\x1b[1;2\x1b"), ended(b"\x1b"));
        // A character half read: the bytes of it that came.
        assert_eq!(around("caf\u{e9}".as_bytes()), open);
        assert_eq!(around(b"caf\xc3"), ended(b"\xc3"));
        assert_eq!(around(b"\xf0\x9f\x98"), ended(b"\xf0\x9f\x98"));
        // A string: ended, never given back — the terminal may already
        // have acted on its body.
        for string in [
            &b"\x1b]2;half a tit"[..],
            b"\x1bPq#0;2;0;0;0",
            b"\x1b_Gf=100,a=T;AAAA",
            b"\x1b]2;title\x1b",
        ] {
            let (interrupt, reopen) = around(string);
            assert_eq!(interrupt, b"\x1b\\", "{string:?}");
            assert_eq!(reopen, None, "{string:?}");
        }
        // A CSI longer than the parameters kept here is no longer all
        // known, so it is not written again either.
        let long = format!("\x1b[{}", "1;".repeat(40));
        assert_eq!(around(long.as_bytes()), (b"\x1b\\".to_vec(), None));
        // And the sequence that fills it is forgotten when it ends.
        let mut o = ModeObserver::new();
        o.observe(long.as_bytes());
        o.observe(b"m\x1b[1;2");
        assert_eq!(o.reopen_sequence().unwrap(), b"\x1b[1;2");
    }

    /// The bytes of a half-read character are forgotten with the rest of
    /// the stream's place in it (acs-xk4, acs-p4u).
    #[test]
    fn resync_forgets_a_half_read_character() {
        let mut o = ModeObserver::new();
        o.observe(b"caf\xc3");
        o.resync();
        assert!(o.at_boundary());
        assert_eq!(o.interrupt_sequence(), b"");
        assert_eq!(o.reopen_sequence().unwrap(), b"");
    }

    #[test]
    fn full_reset_clears_state() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b[?1049h\x1b[?1000h\x1bc");
        assert!(o.is_clean());
    }

    #[test]
    fn kitty_set_in_place_and_modify_other_keys_off() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b[=5;1u\x1b[>4;1m");
        assert_eq!(o.reset_sequence(), b"\x1b[=0;1u\x1b[>4;0m\x1b[0m");
        o.observe(b"\x1b[=0;1u\x1b[>4m");
        assert!(o.is_clean());
    }

    #[test]
    fn untracked_modes_are_ignored() {
        let mut o = ModeObserver::new();
        o.observe(b"\x1b[?7l\x1b[?12h\x1b[4h\x1b[?99999h");
        assert!(o.is_clean());
    }
}
