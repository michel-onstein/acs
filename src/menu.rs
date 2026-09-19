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
    /// title; the sessions as `--list` shows them, the first nine numbered;
    /// the new-session and exit rows; the keys; the note. The cursor's row
    /// is marked and reversed; the rows scroll to keep it in view, and no
    /// line is longer than the width, so nothing wraps.
    pub fn render(&self, host: &str, now: u64, cols: usize, height: usize) -> String {
        let table = crate::list::lines(&self.sessions, now);
        let rows = self.rows();
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
            let (key, text) = match row {
                Row::Session(i) if n < 9 => ((b'1' + n as u8) as char, table[i + 1].as_str()),
                Row::Session(i) => (' ', table[i + 1].as_str()),
                Row::New => ('n', "new session"),
                Row::Exit => (' ', "exit"),
            };
            let here = n == self.cursor;
            let mark = if here { '>' } else { ' ' };
            lines.push((format!("{mark} {key}  {text}"), here));
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
            let text: String = text.chars().take(cols).collect();
            if *here {
                out.push_str(&format!("\x1b[7m{text}\x1b[0m"));
            } else {
                out.push_str(&text);
            }
            out.push_str("\x1b[K");
        }
        out.push_str("\x1b[J");
        out
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
