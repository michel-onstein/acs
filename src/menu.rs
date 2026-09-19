//! The session menu of a plain `acs <host>` (DESIGN §4.4).
//!
//! [`Menu`] is a pure state machine over the bytes typed and an injected
//! millisecond clock: keys in, a [`Choice`] out, and [`Menu::render`] for
//! the screen that shows it. `pick.rs` runs it on the terminal.

use crate::proto::StatusInfo;

/// How long a lone ESC waits for the rest of an arrow key before it is the
/// Esc key — as long as the session's input holds an incomplete sequence
/// (DESIGN §6.3).
pub const ESC_WAIT_MS: u64 = 100;

/// Longest escape sequence read before it is dropped as garbage.
const MAX_SEQ: usize = 32;

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Attach to this session; `force` takes it over from the client
    /// attached to it (DESIGN §4.5).
    Attach { name: String, force: bool },
    /// Create a new session.
    New,
    /// End this session on the host; the menu then goes on.
    Kill(String),
    /// Leave the menu with this exit status.
    Leave(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// An index into the sessions.
    Session(usize),
    New,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Esc,
    Byte(u8),
    /// A sequence the menu has no use for (another cursor key, a function
    /// key).
    Other,
}

/// A question waiting for its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    /// End this session? `y`, or `x` again.
    Kill(String),
    /// Take this attached session over? `y`.
    Takeover(String),
}

pub struct Menu {
    sessions: Vec<StatusInfo>,
    /// Attached sessions are shown too (`.`).
    all: bool,
    /// Take over without asking (`--force`).
    force: bool,
    /// Index into [`Menu::rows`].
    cursor: usize,
    ask: Option<Ask>,
    /// An escape sequence being read, and when its ESC came.
    seq: Vec<u8>,
    seq_since: u64,
    /// The line under the menu: a question, or what the last action did.
    note: String,
}

impl Menu {
    pub fn new(sessions: Vec<StatusInfo>, force: bool) -> Menu {
        Menu {
            sessions,
            all: false,
            force,
            cursor: 0,
            ask: None,
            seq: Vec::new(),
            seq_since: 0,
            note: String::new(),
        }
    }

    pub fn sessions(&self) -> &[StatusInfo] {
        &self.sessions
    }

    pub fn set_note(&mut self, note: String) {
        self.note = note;
    }

    /// The host's sessions changed (one was ended): the cursor stays on its
    /// row, or on the row that took its place.
    pub fn set_sessions(&mut self, sessions: Vec<StatusInfo>) {
        self.keep_cursor(|m| m.sessions = sessions);
    }

    /// The shown sessions — detached only, unless `.` — then a row to
    /// create a new session and one to leave.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = (self.sessions.iter().enumerate())
            .filter(|(_, s)| self.all || !s.attached)
            .map(|(i, _)| Row::Session(i))
            .collect();
        rows.extend([Row::New, Row::Exit]);
        rows
    }

    /// Change what is shown with the cursor on the same row if it is still
    /// there, else at the same place.
    fn keep_cursor(&mut self, change: impl FnOnce(&mut Menu)) {
        let at = self.rows()[self.cursor];
        let name = match at {
            Row::Session(i) => Some(self.sessions[i].name.clone()),
            _ => None,
        };
        change(self);
        let rows = self.rows();
        self.cursor = (rows.iter())
            .position(|&r| match r {
                Row::Session(i) => name.as_ref() == Some(&self.sessions[i].name),
                other => other == at,
            })
            .unwrap_or(self.cursor.min(rows.len() - 1));
    }

    /// When [`Menu::tick`] must run: a lone ESC is waiting.
    pub fn deadline(&self) -> Option<u64> {
        (!self.seq.is_empty()).then_some(self.seq_since + ESC_WAIT_MS)
    }

    /// The clock moved on: an ESC nothing followed is the Esc key.
    pub fn tick(&mut self, now: u64) -> Option<Choice> {
        if self.deadline().is_some_and(|d| now >= d) {
            let lone = self.seq.len() == 1;
            self.seq.clear();
            if lone {
                return self.key(Key::Esc);
            }
        }
        None
    }

    /// Bytes typed. The first choice ends the feed: what follows it was
    /// typed ahead of a screen that is about to go.
    pub fn feed(&mut self, input: &[u8], now: u64) -> Option<Choice> {
        for &b in input {
            let key = if self.seq.is_empty() && b != 0x1b {
                Key::Byte(b)
            } else {
                self.seq.push(b);
                if self.seq.len() == 1 {
                    self.seq_since = now;
                }
                match self.sequence() {
                    Some(k) => k,
                    None => continue,
                }
            };
            if let Some(c) = self.key(key) {
                self.seq.clear();
                return Some(c);
            }
        }
        None
    }

    /// The key `seq` spells once it is whole (emptying it); `None` while
    /// more is to come. An ESC followed by anything but `[` or `O` is the
    /// Esc key (and the rest, an Alt+key, goes with it).
    fn sequence(&mut self) -> Option<Key> {
        let s = &self.seq;
        let key = match s.get(1) {
            None => return None,
            Some(b'[') => match s.last() {
                Some(&f) if s.len() > 2 && (0x40..=0x7e).contains(&f) => arrow(f),
                _ if s.len() < MAX_SEQ => return None,
                _ => Key::Other,
            },
            Some(b'O') => match s.get(2) {
                Some(&f) => arrow(f),
                None => return None,
            },
            Some(_) => Key::Esc,
        };
        self.seq.clear();
        Some(key)
    }

    fn key(&mut self, key: Key) -> Option<Choice> {
        match key {
            // Esc and Ctrl-C leave at any point, a question pending or not.
            Key::Esc => return Some(Choice::Leave(0)),
            Key::Byte(0x03) => return Some(Choice::Leave(130)),
            Key::Other => return None,
            _ => {}
        }
        self.note.clear();
        if let Some(ask) = self.ask.take() {
            // Any other key is a no.
            return match (ask, key) {
                (Ask::Kill(name), Key::Byte(b'y' | b'Y' | b'x')) => Some(Choice::Kill(name)),
                (Ask::Takeover(name), Key::Byte(b'y' | b'Y')) => {
                    Some(Choice::Attach { name, force: true })
                }
                _ => None,
            };
        }
        let rows = self.rows();
        match key {
            Key::Up | Key::Byte(b'k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Byte(b'j') => self.cursor = (self.cursor + 1).min(rows.len() - 1),
            Key::Byte(b'\r' | b'\n') => return self.choose(rows[self.cursor]),
            // Sessions come first, so the n-th row is the n-th session.
            Key::Byte(d @ b'1'..=b'9') => {
                let n = (d - b'1') as usize;
                if let Some(&row @ Row::Session(_)) = rows.get(n) {
                    self.cursor = n;
                    return self.choose(row);
                }
            }
            Key::Byte(b'.') => self.keep_cursor(|m| m.all = !m.all),
            Key::Byte(b'x') => {
                if let Row::Session(i) = rows[self.cursor] {
                    let s = &self.sessions[i];
                    let whose = match s.attached {
                        true => format!(", attached from {},", s.identity),
                        false => String::new(),
                    };
                    self.note = format!(
                        "end session '{}'{whose}? y (or x) ends it, any other key keeps it",
                        s.name
                    );
                    self.ask = Some(Ask::Kill(s.name.clone()));
                }
            }
            Key::Byte(b'n') => return Some(Choice::New),
            _ => {}
        }
        None
    }

    /// Enter on `row`, or its number.
    fn choose(&mut self, row: Row) -> Option<Choice> {
        let i = match row {
            Row::New => return Some(Choice::New),
            Row::Exit => return Some(Choice::Leave(0)),
            Row::Session(i) => i,
        };
        let s = &self.sessions[i];
        if !s.attached || self.force {
            return Some(Choice::Attach {
                name: s.name.clone(),
                force: s.attached,
            });
        }
        self.note = format!(
            "session '{}' is attached from {} — take over? [y/N]",
            s.name, s.identity
        );
        self.ask = Some(Ask::Takeover(s.name.clone()));
        None
    }

    /// The whole screen, for a terminal `cols` wide and `height` high: a
    /// title; the sessions as `acs list` shows them, the first nine numbered;
    /// the new-session and exit rows; the keys; the note. The cursor's row
    /// is marked and reversed, in a bar as wide as the widest row of the
    /// list, so it keeps its width as it moves; the rows scroll to keep it
    /// in view, and no line is wider than the terminal, so nothing wraps.
    pub fn render(&self, host: &str, now: u64, cols: usize, height: usize) -> String {
        let table = crate::list::lines(&self.sessions, now);
        let rows = self.rows();
        let label = |row: Row| match row {
            Row::Session(i) => table[i + 1].as_str(),
            Row::New => "new session",
            Row::Exit => "exit",
        };
        // Every row is `{mark} {key}  {label}`.
        let bar = rows
            .iter()
            .map(|&r| 5 + width(label(r)))
            .max()
            .unwrap_or(0)
            .min(cols);
        let fit = height.saturating_sub(6).max(1);
        let first = (self.cursor + 1).saturating_sub(fit);
        let mut lines: Vec<(String, bool)> = vec![
            (
                format!(
                    "acs: {} sessions on {host}",
                    if self.all { "all" } else { "detached" }
                ),
                false,
            ),
            (String::new(), false),
            (format!("     {}", table[0]), false),
        ];
        for (n, &row) in rows.iter().enumerate().skip(first).take(fit) {
            let key = match row {
                Row::Session(_) if n < 9 => (b'1' + n as u8) as char,
                Row::New => 'n',
                Row::Session(_) | Row::Exit => ' ',
            };
            let here = n == self.cursor;
            let mark = if here { '>' } else { ' ' };
            lines.push((format!("{mark} {key}  {}", label(row)), here));
        }
        lines.push((String::new(), false));
        lines.push((
            format!(
                "1-9, or ↑↓ jk and Enter: attach   .: {}   x: end   n: new   Esc: leave",
                if self.all { "detached only" } else { "all" }
            ),
            false,
        ));
        lines.push((self.note.clone(), false));
        let mut out = String::from("\x1b[H");
        for (i, (text, here)) in lines.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            let text = clip(text, cols);
            if *here {
                let pad = bar.saturating_sub(width(text));
                out.push_str(&format!("\x1b[7m{text}{:pad$}\x1b[0m", ""));
            } else {
                out.push_str(text);
            }
            out.push_str("\x1b[K");
        }
        out.push_str("\x1b[J");
        out
    }
}

/// How many terminal columns `s` takes: see [`char_width`].
fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// The longest start of `s` that fits in `cols` columns.
fn clip(s: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += char_width(c);
        if used > cols {
            return &s[..i];
        }
    }
    s
}

/// Columns a character takes in a terminal, as `wcwidth` has it for the
/// common cases: none for combining marks, zero-width characters and
/// variation selectors; two for East Asian wide and fullwidth characters
/// and emoji; one otherwise. A session's command (from `acs list`) may hold
/// any of them.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036F
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x200B..=0x200F
        | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// The cursor key a final byte (of `CSI … A` or `SS3 A`) stands for.
fn arrow(f: u8) -> Key {
    match f {
        b'A' => Key::Up,
        b'B' => Key::Down,
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, attached: bool) -> StatusInfo {
        StatusInfo {
            name: name.into(),
            attached,
            identity: "alice@laptop".into(),
            command: "sh".into(),
            ..Default::default()
        }
    }

    /// `main` and `work` detached, `busy` attached.
    fn menu() -> Menu {
        Menu::new(
            vec![info("main", false), info("busy", true), info("work", false)],
            false,
        )
    }

    fn attach(name: &str) -> Option<Choice> {
        Some(Choice::Attach {
            name: name.into(),
            force: false,
        })
    }

    #[test]
    fn a_number_attaches_its_session() {
        assert_eq!(menu().feed(b"1", 0), attach("main"));
        // Attached sessions are hidden, so work is the second.
        assert_eq!(menu().feed(b"2", 0), attach("work"));
        // No third session: nothing happens.
        let mut m = menu();
        assert_eq!(m.feed(b"3", 0), None);
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn the_cursor_moves_with_arrows_and_vim_keys_and_enter_chooses() {
        for down in [&b"j"[..], b"\x1b[B", b"\x1bOB"] {
            let mut m = menu();
            assert_eq!(m.feed(down, 0), None, "{down:?}");
            assert_eq!(m.feed(b"\r", 0), attach("work"), "{down:?}");
        }
        let mut m = menu();
        m.feed(b"jjk", 0);
        assert_eq!(m.feed(b"\x1b[A\r", 0), attach("main"));
        // The cursor stops at the ends: exit is the last row.
        let mut m = menu();
        m.feed(b"kkk", 0);
        assert_eq!(m.cursor, 0);
        m.feed(b"jjjjjjj", 0);
        assert_eq!(m.feed(b"\r", 0), Some(Choice::Leave(0)));
    }

    #[test]
    fn the_new_session_row_and_n_create_one() {
        let mut m = menu();
        m.feed(b"jj", 0);
        assert_eq!(m.feed(b"\r", 0), Some(Choice::New));
        assert_eq!(menu().feed(b"n", 0), Some(Choice::New));
    }

    #[test]
    fn esc_leaves_once_nothing_follows_it() {
        let mut m = menu();
        assert_eq!(m.feed(b"\x1b", 1000), None);
        assert_eq!(m.deadline(), Some(1000 + ESC_WAIT_MS));
        assert_eq!(m.tick(1050), None);
        assert_eq!(m.tick(1000 + ESC_WAIT_MS), Some(Choice::Leave(0)));
        // Esc leaves during a question too.
        let mut m = menu();
        m.feed(b"x", 0);
        m.feed(b"\x1b", 0);
        assert_eq!(m.tick(ESC_WAIT_MS), Some(Choice::Leave(0)));
        // ESC then another key at once is Esc as well (Alt+key).
        assert_eq!(menu().feed(b"\x1bj", 0), Some(Choice::Leave(0)));
    }

    #[test]
    fn an_arrow_split_across_reads_is_not_esc() {
        let mut m = menu();
        assert_eq!(m.feed(b"\x1b", 0), None);
        assert_eq!(m.feed(b"[", 40), None);
        assert_eq!(m.feed(b"B", 60), None);
        assert_eq!(m.deadline(), None);
        assert_eq!(m.tick(1000), None);
        assert_eq!(m.cursor, 1);
        // An incomplete sequence the clock runs out on is dropped, not Esc.
        let mut m = menu();
        m.feed(b"\x1b[1;", 0);
        assert_eq!(m.tick(ESC_WAIT_MS), None);
        assert_eq!(m.feed(b"1", 200), attach("main"));
        // Other keys' sequences do nothing.
        let mut m = menu();
        assert_eq!(m.feed(b"\x1b[C\x1b[15~\x1bOP", 0), None);
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn ctrl_c_leaves_with_130() {
        assert_eq!(menu().feed(b"\x03", 0), Some(Choice::Leave(130)));
    }

    #[test]
    fn dot_shows_attached_sessions_and_taking_one_over_asks_first() {
        let mut m = menu();
        m.feed(b"j", 0); // on work
        m.feed(b".", 0);
        // Still on work, now the third row.
        assert_eq!(m.cursor, 2);
        assert_eq!(m.feed(b"2", 0), None);
        assert!(
            m.note.contains("'busy' is attached from alice@laptop"),
            "{}",
            m.note
        );
        assert_eq!(
            m.feed(b"y", 0),
            Some(Choice::Attach {
                name: "busy".into(),
                force: true
            })
        );
        // Any other key is a no.
        let mut m = menu();
        m.feed(b".2", 0);
        assert_eq!(m.feed(b"n", 0), None);
        assert!(m.note.is_empty());
        // `.` again hides them.
        m.feed(b".", 0);
        assert_eq!(m.feed(b"2", 0), attach("work"));
    }

    #[test]
    fn force_takes_over_without_asking() {
        let mut m = Menu::new(vec![info("busy", true)], true);
        assert_eq!(
            m.feed(b".1", 0),
            Some(Choice::Attach {
                name: "busy".into(),
                force: true
            })
        );
    }

    #[test]
    fn x_asks_before_ending_a_session() {
        let mut m = menu();
        m.feed(b"jx", 0);
        assert!(m.note.contains("end session 'work'?"), "{}", m.note);
        assert_eq!(m.feed(b"y", 0), Some(Choice::Kill("work".into())));
        // x twice is a yes too.
        assert_eq!(menu().feed(b"xx", 0), Some(Choice::Kill("main".into())));
        // Anything else keeps it.
        let mut m = menu();
        assert_eq!(m.feed(b"xk", 0), None);
        assert!(m.note.is_empty());
        // Not on the new-session or exit rows.
        let mut m = menu();
        m.feed(b"jjx", 0);
        assert_eq!(m.feed(b"y", 0), None);
    }

    #[test]
    fn the_menu_follows_an_ended_session() {
        let mut m = menu();
        m.feed(b"j", 0); // on work
        m.set_sessions(vec![info("busy", true), info("work", false)]);
        assert_eq!(m.cursor, 0, "still on work");
        m.set_sessions(vec![info("busy", true)]);
        // work went: the cursor is on what took its row, the new-session one.
        assert_eq!(m.feed(b"\r", 0), Some(Choice::New));
    }

    #[test]
    fn more_than_nine_sessions_number_the_first_nine() {
        let names: Vec<String> = (1..=12).map(|n| format!("s{n}")).collect();
        let mut m = Menu::new(names.iter().map(|n| info(n, false)).collect(), false);
        assert_eq!(m.feed(b"9", 0), attach("s9"));
        let mut m = Menu::new(names.iter().map(|n| info(n, false)).collect(), false);
        m.feed(&[b'j'; 11], 0);
        assert_eq!(m.feed(b"\r", 0), attach("s12"));
        let screen = m.render("devbox", 0, 80, 40);
        assert!(screen.contains("  9  s9 "), "{screen:?}");
        assert!(screen.contains("     s10 "), "{screen:?}");
    }

    #[test]
    fn the_screen_shows_the_list_the_cursor_and_the_note() {
        let mut m = menu();
        m.feed(b"jx", 0);
        let screen = m.render("devbox", 0, 80, 24);
        let lines: Vec<&str> = screen.split("\r\n").collect();
        assert!(lines[0].starts_with("\x1b[Hacs: detached sessions on devbox"));
        assert!(lines[2].starts_with("     NAME  STATE"), "{lines:?}");
        assert!(
            lines[3].starts_with("  1  main  detached  (alice@laptop)"),
            "{lines:?}"
        );
        assert!(
            lines[4].starts_with("\x1b[7m> 2  work  detached"),
            "{lines:?}"
        );
        assert!(lines[5].starts_with("  n  new session"), "{lines:?}");
        assert!(lines[6].starts_with("     exit"), "{lines:?}");
        assert!(lines[9].starts_with("end session 'work'?"), "{lines:?}");
        assert!(screen.ends_with("\x1b[K\x1b[J"));
        // Nothing longer than the terminal is wide.
        let narrow = m.render("devbox", 0, 12, 24);
        for l in narrow.split("\r\n") {
            let text = l.replace("\x1b[H", "").replace("\x1b[7m", "");
            let text = text.split('\x1b').next().unwrap();
            assert!(text.chars().count() <= 12, "{l:?}");
        }
    }

    /// The reversed bar on a screen: the cursor's row, padding included.
    fn bar(screen: &str) -> &str {
        let start = screen.find("\x1b[7m").expect("no bar") + 4;
        let len = screen[start..].find("\x1b[0m").expect("an open bar");
        &screen[start..start + len]
    }

    /// acs-km8: the bar keeps one width as the cursor moves — the widest
    /// row's, or the terminal's if that is narrower.
    #[test]
    fn the_bar_is_as_wide_as_the_widest_row_on_every_row() {
        let mut long = info("work", false);
        long.command = "htop --delay 10 --sort-key PERCENT_CPU".into();
        let mut m = Menu::new(vec![info("main", false), long], false);
        // main, work, new session, exit.
        let mut bars = Vec::new();
        let mut narrow = Vec::new();
        for _ in 0..4 {
            bars.push(bar(&m.render("devbox", 0, 120, 24)).to_string());
            narrow.push(bar(&m.render("devbox", 0, 30, 24)).to_string());
            m.feed(b"j", 0);
        }
        let widest = width(&bars[1]);
        assert!(bars[1].ends_with("PERCENT_CPU"), "{:?}", bars[1]);
        assert!(bars[1].starts_with("> 2  work"), "{:?}", bars[1]);
        for b in &bars {
            assert_eq!(width(b), widest, "{bars:?}");
        }
        // The short rows are padded with spaces to it.
        assert!(bars[3].starts_with(">    exit   "), "{:?}", bars[3]);
        assert_eq!(bars[3].trim_end(), ">    exit");
        assert!(bars[0].starts_with("> 1  main"), "{:?}", bars[0]);
        // Narrower than the widest row: the terminal's width, and no wrap.
        for b in &narrow {
            assert_eq!(width(b), 30, "{narrow:?}");
        }
    }

    #[test]
    fn widths_count_terminal_columns() {
        assert_eq!(width("exit"), 4);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(width("🦀x"), 3);
        assert_eq!(clip("日本語", 5), "日本");
        assert_eq!(clip("日本語", 6), "日本語");
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abc", 0), "");
        // A wide command is clipped and padded in columns, not characters.
        let mut wide = info("jp", false);
        wide.command = "vim 日本語.txt".into();
        let m = Menu::new(vec![wide, info("main", false)], false);
        let b = m.render("devbox", 0, 80, 24);
        let widest = 5 + width(crate::list::lines(m.sessions(), 0)[1].as_str());
        assert_eq!(width(bar(&b)), widest, "{:?}", bar(&b));
        let clipped = m.render("devbox", 0, 20, 24);
        assert!(width(bar(&clipped)) <= 20, "{:?}", bar(&clipped));
    }

    #[test]
    fn a_short_screen_scrolls_to_the_cursor() {
        let names: Vec<String> = (1..=20).map(|n| format!("s{n}")).collect();
        let mut m = Menu::new(names.iter().map(|n| info(n, false)).collect(), false);
        m.feed(&[b'j'; 15], 0);
        let screen = m.render("devbox", 0, 80, 10);
        assert_eq!(screen.split("\r\n").count(), 10, "{screen:?}");
        assert!(screen.contains("\x1b[7m>    s16 "), "{screen:?}");
        assert!(!screen.contains("s1 "), "{screen:?}");
    }
}
