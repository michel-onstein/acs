//! The session menu of a plain `acs <host>` (DESIGN §4.4), and of
//! `acs list` on every host alias at once (§7.3).
//!
//! [`Menu`] is a pure state machine over the bytes typed and an injected
//! millisecond clock: keys in, a [`Choice`] out, and [`Menu::render`] for
//! the screen that shows it. `pick.rs` runs it on the terminal. Its rows are
//! grouped by host: one for `acs <host>`, every alias in configuration order
//! for `acs list`, where each host's rows arrive as it answers.

use crate::proto::StatusInfo;

/// How long a lone ESC waits for the rest of an arrow key before it is the
/// Esc key — as long as the session's input holds an incomplete sequence
/// (DESIGN §6.3).
pub const ESC_WAIT_MS: u64 = 100;

/// Longest escape sequence read before it is dropped as garbage.
const MAX_SEQ: usize = 32;

/// What the user chose. `host` is an index into the menu's hosts (always 0
/// in the menu of one host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Attach to this session; `force` takes it over from the client
    /// attached to it (DESIGN §4.5).
    Attach {
        host: usize,
        name: String,
        force: bool,
    },
    /// Create a new session on this host.
    New { host: usize },
    /// End this session on the host; the menu then goes on.
    Kill { host: usize, name: String },
    /// Leave the menu with this exit status.
    Leave(u8),
}

/// What a host said to the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Not yet: its rows come when it answers.
    Asking,
    /// Its sessions, perhaps none.
    Sessions(Vec<StatusInfo>),
    /// No sessions to offer, and the line `acs list` prints instead (not
    /// installed, unreachable).
    Line(String),
}

struct Host {
    name: String,
    answer: Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A host, and an index into its sessions.
    Session(usize, usize),
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

/// A question waiting for its answer, about a host's session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    /// End this session? `y`, or `x` again.
    Kill(usize, String),
    /// Take this attached session over? `y`.
    Takeover(usize, String),
}

pub struct Menu {
    hosts: Vec<Host>,
    /// Every host alias (`acs list`): a HOST column, no new-session row.
    every: bool,
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
    /// The menu of one host's `sessions`.
    pub fn new(sessions: Vec<StatusInfo>, force: bool) -> Menu {
        Menu::with_hosts(
            vec![Host {
                name: String::new(),
                answer: Answer::Sessions(sessions),
            }],
            false,
            force,
        )
    }

    /// The menu of every host alias, `names` in configuration order, each
    /// asked still ([`Menu::set_answer`] as it answers).
    pub fn every_host(names: Vec<String>, force: bool) -> Menu {
        let hosts = names
            .into_iter()
            .map(|name| Host {
                name,
                answer: Answer::Asking,
            })
            .collect();
        Menu::with_hosts(hosts, true, force)
    }

    fn with_hosts(hosts: Vec<Host>, every: bool, force: bool) -> Menu {
        Menu {
            hosts,
            every,
            all: false,
            force,
            cursor: 0,
            ask: None,
            seq: Vec::new(),
            seq_since: 0,
            note: String::new(),
        }
    }

    /// Host `host`'s sessions (none until it has answered).
    pub fn sessions(&self, host: usize) -> &[StatusInfo] {
        match &self.hosts[host].answer {
            Answer::Sessions(s) => s,
            _ => &[],
        }
    }

    /// Host `host`'s name, as configured (empty in the menu of one host).
    pub fn host_name(&self, host: usize) -> &str {
        &self.hosts[host].name
    }

    pub fn set_note(&mut self, note: String) {
        self.note = note;
    }

    /// A host's sessions changed (one was ended): the cursor stays on its
    /// row, or on the row that took its place.
    pub fn set_sessions(&mut self, host: usize, sessions: Vec<StatusInfo>) {
        self.set_answer(host, Answer::Sessions(sessions));
    }

    /// A host answered (or failed to): its rows come in, the cursor stays
    /// on its row — except on an exit row with nothing above it yet, the
    /// first thing shown while every host is still being asked: then the
    /// first rows to come take the cursor.
    pub fn set_answer(&mut self, host: usize, answer: Answer) {
        let waiting = self.cursor == 0 && self.rows()[0] == Row::Exit;
        self.keep_cursor(|m| m.hosts[host].answer = answer);
        if waiting {
            self.cursor = 0;
        }
    }

    /// The shown sessions, host by host — detached only, unless `.` — then
    /// a row to create a new session (one host only) and one to leave.
    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        for h in 0..self.hosts.len() {
            rows.extend(
                (self.sessions(h).iter().enumerate())
                    .filter(|(_, s)| self.all || !s.attached)
                    .map(|(i, _)| Row::Session(h, i)),
            );
        }
        if !self.every {
            rows.push(Row::New);
        }
        rows.push(Row::Exit);
        rows
    }

    /// Change what is shown with the cursor on the same row if it is still
    /// there, else at the same place.
    fn keep_cursor(&mut self, change: impl FnOnce(&mut Menu)) {
        let at = self.rows()[self.cursor];
        let name = match at {
            Row::Session(h, i) => Some((h, self.sessions(h)[i].name.clone())),
            _ => None,
        };
        change(self);
        let rows = self.rows();
        self.cursor = (rows.iter())
            .position(|&r| match r {
                Row::Session(h, i) => name.as_ref() == Some(&(h, self.sessions(h)[i].name.clone())),
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
                (Ask::Kill(host, name), Key::Byte(b'y' | b'Y' | b'x')) => {
                    Some(Choice::Kill { host, name })
                }
                (Ask::Takeover(host, name), Key::Byte(b'y' | b'Y')) => Some(Choice::Attach {
                    host,
                    name,
                    force: true,
                }),
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
                if let Some(&row @ Row::Session(..)) = rows.get(n) {
                    self.cursor = n;
                    return self.choose(row);
                }
            }
            Key::Byte(b'.') => self.keep_cursor(|m| m.all = !m.all),
            // acs-7zb: x and n act on the row under the cursor, so off one
            // they act on they do nothing but say what they would need.
            Key::Byte(b'x') => match rows[self.cursor] {
                Row::Session(h, i) => {
                    let s = self.sessions(h)[i].clone();
                    let whose = match s.attached {
                        true => format!(", attached from {},", s.identity),
                        false => String::new(),
                    };
                    self.note = format!(
                        "end session '{}'{}{whose}? y (or x) ends it, any other key keeps it",
                        s.name,
                        self.on(h)
                    );
                    self.ask = Some(Ask::Kill(h, s.name.clone()));
                }
                _ => self.note = "x ends the session under the cursor".into(),
            },
            Key::Byte(b'n') => match (self.every, rows[self.cursor]) {
                (false, Row::Session(..) | Row::New) => return Some(Choice::New { host: 0 }),
                (true, Row::Session(host, _)) => return Some(Choice::New { host }),
                (true, _) => {
                    self.note =
                        "n makes a new session on the host of the session under the cursor".into()
                }
                (false, _) => self.note = "the 'new session' row above makes a new session".into(),
            },
            _ => {}
        }
        None
    }

    /// ` on <host>` in a message of the menu of every host; nothing in the
    /// menu of one.
    fn on(&self, host: usize) -> String {
        match self.every {
            true => format!(" on {}", self.hosts[host].name),
            false => String::new(),
        }
    }

    /// Enter on `row`, or its number.
    fn choose(&mut self, row: Row) -> Option<Choice> {
        let (h, i) = match row {
            Row::New => return Some(Choice::New { host: 0 }),
            Row::Exit => return Some(Choice::Leave(0)),
            Row::Session(h, i) => (h, i),
        };
        let s = self.sessions(h)[i].clone();
        if !s.attached || self.force {
            return Some(Choice::Attach {
                host: h,
                name: s.name,
                force: s.attached,
            });
        }
        self.note = format!(
            "session '{}'{} is attached from {} — take over? [y/N]",
            s.name,
            self.on(h),
            s.identity
        );
        self.ask = Some(Ask::Takeover(h, s.name.clone()));
        None
    }

    /// The whole screen, for a terminal `cols` wide and `height` high: a
    /// title; the sessions as `acs list` shows them, the first nine numbered;
    /// the new-session and exit rows; the keys; the note. The cursor's row
    /// is marked and reversed, in a bar as wide as the widest row of the
    /// list, so it keeps its width as it moves; the rows scroll to keep it
    /// in view, and no line is wider than the terminal, so nothing wraps.
    ///
    /// The menu of every host titles itself with "every host" (`host` is not
    /// used), puts a HOST column first, and lists under the rows, as
    /// `acs list` does, each host that has no row to offer and why: still
    /// being asked, no sessions (or only attached ones while they are
    /// hidden), not installed, unreachable.
    pub fn render(&self, host: &str, now: u64, cols: usize, height: usize) -> String {
        // The table of every session, heading first; each host's sessions
        // start at its offset in it.
        let (table, offsets) = if self.every {
            let listed: Vec<(&str, &[StatusInfo])> = (0..self.hosts.len())
                .map(|h| (self.hosts[h].name.as_str(), self.sessions(h)))
                .collect();
            let mut offsets = Vec::new();
            let mut at = 0;
            for (_, s) in &listed {
                offsets.push(at);
                at += s.len();
            }
            (crate::list::host_lines(&listed, now), offsets)
        } else {
            (crate::list::lines(self.sessions(0), now), vec![0])
        };
        let rows = self.rows();
        let label = |row: Row| match row {
            Row::Session(h, i) => table[offsets[h] + i + 1].as_str(),
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
        let info = self.host_lines();
        let spare = if info.is_empty() { 0 } else { info.len() + 1 };
        let fit = height.saturating_sub(6 + spare).max(1);
        let first = (self.cursor + 1).saturating_sub(fit);
        let shown = if self.all { "all" } else { "detached" };
        let title = match self.every {
            true => format!("acs: {shown} sessions on every host"),
            false => format!("acs: {shown} sessions on {host}"),
        };
        let mut lines: Vec<(String, bool)> = vec![
            (title, false),
            (String::new(), false),
            (format!("     {}", table[0]), false),
        ];
        for (n, &row) in rows.iter().enumerate().skip(first).take(fit) {
            let key = match row {
                Row::Session(..) if n < 9 => (b'1' + n as u8) as char,
                Row::New => 'n',
                Row::Session(..) | Row::Exit => ' ',
            };
            let here = n == self.cursor;
            let mark = if here { '>' } else { ' ' };
            lines.push((format!("{mark} {key}  {}", label(row)), here));
        }
        if !info.is_empty() {
            lines.push((String::new(), false));
            lines.extend(info.into_iter().map(|l| (format!("     {l}"), false)));
        }
        lines.push((String::new(), false));
        lines.push((self.keys(&rows), false));
        lines.push((self.note.clone(), false));
        let mut out = String::from("\x1b[H");
        for (i, (text, here)) in lines.iter().enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            // Rows, and the note under them, are built from names and
            // identities the remote chose. The menu is acs's own drawing,
            // so nothing in a line may move the cursor or erase a
            // neighbour: a row that redraws the rows around it is a row the
            // user attaches to, or ends, by mistake (acs-w1z). Clipping
            // below is by width and would not stop it.
            let text = crate::safe::display_max(text, cols.max(crate::safe::MAX_FIELD));
            let text = clip(&text, cols);
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

    /// The key bar at the foot (acs-7zb): only the keys that act on the row
    /// under the cursor, so that x and n are not offered where they do
    /// nothing, and Enter is named for what it does there.
    fn keys(&self, rows: &[Row]) -> String {
        let shown = if self.all { "detached only" } else { "all" };
        let mut keys: Vec<String> = Vec::new();
        match rows[self.cursor] {
            // Every key acts here, so the bar reads as it always has.
            Row::Session(..) => {
                keys.push("1-9, or ↑↓ jk and Enter: attach".into());
                keys.push(format!(".: {shown}"));
                keys.push("x: end".into());
                keys.push(format!("n: new{}", self.there()));
            }
            Row::New => {
                if rows.iter().any(|r| matches!(r, Row::Session(..))) {
                    keys.push("1-9: attach".into());
                }
                keys.push("↑↓ jk: move".into());
                keys.push("Enter or n: new session".into());
                keys.push(format!(".: {shown}"));
            }
            Row::Exit => {
                if rows.iter().any(|r| matches!(r, Row::Session(..))) {
                    keys.push("1-9: attach".into());
                }
                if rows.len() > 1 {
                    keys.push("↑↓ jk: move".into());
                }
                keys.push(format!(".: {shown}"));
                keys.push("Enter or Esc: leave".into());
            }
        }
        if !matches!(rows[self.cursor], Row::Exit) {
            keys.push("Esc: leave".into());
        }
        keys.join("   ")
    }

    /// ` there` for `n` in the menu of every host, where the new session is
    /// made on the host of the session under the cursor; nothing in the
    /// menu of one.
    fn there(&self) -> &'static str {
        match self.every {
            true => " there",
            false => "",
        }
    }

    /// In the menu of every host, a line for each host without a row shown,
    /// in configuration order; none in the menu of one.
    fn host_lines(&self) -> Vec<String> {
        if !self.every {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for h in &self.hosts {
            match &h.answer {
                Answer::Asking => lines.push(format!("asking {}…", h.name)),
                Answer::Line(l) => lines.push(l.clone()),
                Answer::Sessions(s) if s.is_empty() => {
                    lines.push(format!("no sessions on {}", h.name))
                }
                Answer::Sessions(s) if !self.all && s.iter().all(|s| s.attached) => {
                    lines.push(format!(
                        "no detached sessions on {} ({} attached: . shows them)",
                        h.name,
                        s.len()
                    ))
                }
                Answer::Sessions(_) => {}
            }
        }
        lines
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
            host: 0,
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
        assert_eq!(m.feed(b"\r", 0), Some(Choice::New { host: 0 }));
        assert_eq!(menu().feed(b"n", 0), Some(Choice::New { host: 0 }));
        // n on the new-session row makes one too: it is that row's key.
        let mut m = menu();
        m.feed(b"jj", 0);
        assert_eq!(m.feed(b"n", 0), Some(Choice::New { host: 0 }));
    }

    /// The key bar the menu draws at the foot: the line before the note.
    fn key_bar(m: &Menu) -> String {
        let screen = m.render("devbox", 0, 200, 30);
        let lines: Vec<String> = screen
            .split("\r\n")
            .map(|l| {
                let l = l.replace("\x1b[H", "").replace("\x1b[7m", "");
                l.split('\x1b').next().unwrap().to_string()
            })
            .collect();
        lines[lines.len() - 2].clone()
    }

    /// acs-7zb: the bar names only the keys that act on the row under the
    /// cursor — no x or n on the new-session and exit rows, and Enter named
    /// for what it does there.
    #[test]
    fn the_key_bar_offers_only_the_keys_for_the_row_under_the_cursor() {
        // A session row: every key acts, and the bar reads as it always has.
        assert_eq!(
            key_bar(&menu()),
            "1-9, or ↑↓ jk and Enter: attach   .: all   x: end   n: new   Esc: leave"
        );
        // The new-session row: no x, and Enter is the row's own action.
        let mut m = menu();
        m.feed(b"jj", 0);
        assert_eq!(
            key_bar(&m),
            "1-9: attach   ↑↓ jk: move   Enter or n: new session   .: all   Esc: leave"
        );
        // The exit row: neither x nor n.
        let mut m = menu();
        m.feed(b"jjj", 0);
        assert_eq!(
            key_bar(&m),
            "1-9: attach   ↑↓ jk: move   .: all   Enter or Esc: leave"
        );
        // The menu of every host before any host answers is the exit row
        // alone: nothing to attach and nowhere to move either.
        assert_eq!(key_bar(&every()), ".: all   Enter or Esc: leave");
    }

    /// acs-7zb: on the exit row n does not make a session and x does not
    /// ask; each only says what it would act on, and nothing else changes.
    #[test]
    fn n_and_x_do_nothing_on_the_exit_row() {
        /// The screen down to the blank line under the rows.
        fn rows(m: &Menu) -> Vec<String> {
            let screen = m.render("devbox", 0, 200, 30);
            screen.split("\r\n").take(8).map(String::from).collect()
        }
        let mut m = menu();
        m.feed(b"jjj", 0);
        let before = rows(&m);
        assert_eq!(m.feed(b"n", 0), None);
        assert_eq!(m.cursor, 3);
        assert_eq!(m.note, "the 'new session' row above makes a new session");
        assert_eq!(m.feed(b"x", 0), None);
        assert!(m.ask.is_none());
        assert_eq!(m.note, "x ends the session under the cursor");
        assert_eq!(rows(&m), before);
        // Enter on the row still leaves.
        assert_eq!(m.feed(b"\r", 0), Some(Choice::Leave(0)));
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
                host: 0,
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
                host: 0,
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
        assert_eq!(
            m.feed(b"y", 0),
            Some(Choice::Kill {
                host: 0,
                name: "work".into()
            })
        );
        // x twice is a yes too.
        assert_eq!(
            menu().feed(b"xx", 0),
            Some(Choice::Kill {
                host: 0,
                name: "main".into()
            })
        );
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
        m.set_sessions(0, vec![info("busy", true), info("work", false)]);
        assert_eq!(m.cursor, 0, "still on work");
        m.set_sessions(0, vec![info("busy", true)]);
        // work went: the cursor is on what took its row, the new-session one.
        assert_eq!(m.feed(b"\r", 0), Some(Choice::New { host: 0 }));
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
        let widest = 5 + width(crate::list::lines(m.sessions(0), 0)[1].as_str());
        assert_eq!(width(bar(&b)), widest, "{:?}", bar(&b));
        let clipped = m.render("devbox", 0, 20, 24);
        assert!(width(bar(&clipped)) <= 20, "{:?}", bar(&clipped));
    }

    /// acs-uxj: devbox, nas, pi and old, as `acs list` asks them.
    fn every() -> Menu {
        Menu::every_host(
            ["devbox", "nas", "pi", "old"].map(String::from).to_vec(),
            false,
        )
    }

    /// The screen's lines after the heading, without the escapes.
    fn shown(m: &Menu) -> Vec<String> {
        let screen = m.render("", 0, 100, 30);
        screen
            .split("\r\n")
            .map(|l| {
                let l = l.replace("\x1b[H", "").replace("\x1b[7m", "");
                l.split('\x1b').next().unwrap().to_string()
            })
            .collect()
    }

    #[test]
    fn every_host_rows_arrive_as_hosts_answer_in_configuration_order() {
        let mut m = every();
        let lines = shown(&m);
        assert_eq!(lines[0], "acs: detached sessions on every host");
        assert!(lines[2].starts_with("     HOST  NAME  STATE"), "{lines:?}");
        // Nothing yet but the exit row and a line per host being asked.
        assert!(lines[3].starts_with(">    exit"), "{lines:?}");
        assert_eq!(lines[5], "     asking devbox…");
        assert_eq!(lines[8], "     asking old…");
        // nas answers first; its session is the first row.
        m.set_answer(1, Answer::Sessions(vec![info("work", false)]));
        assert!(shown(&m)[3].starts_with("> 1  nas   work  detached"));
        // devbox answers: its rows come first, in configuration order, and
        // the cursor stays on nas's work.
        m.set_answer(
            0,
            Answer::Sessions(vec![info("main", false), info("busy", true)]),
        );
        m.set_answer(2, Answer::Line("no sessions on pi (not installed)".into()));
        m.set_answer(
            3,
            Answer::Line("old: no host for 'old' is reachable".into()),
        );
        let lines = shown(&m);
        assert!(
            lines[3].starts_with("  1  devbox  main  detached"),
            "{lines:?}"
        );
        assert!(
            lines[4].starts_with("> 2  nas     work  detached"),
            "{lines:?}"
        );
        assert!(lines[5].starts_with("     exit"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("new session")),
            "{lines:?}"
        );
        assert_eq!(
            lines[7..9],
            [
                "     no sessions on pi (not installed)",
                "     old: no host for 'old' is reachable",
            ]
        );
        assert!(lines[10].contains("n: new there"), "{lines:?}");
        // A host with only attached sessions says so until `.` shows them.
        let mut m = every();
        m.set_answer(0, Answer::Sessions(vec![info("busy", true)]));
        m.set_answer(1, Answer::Sessions(vec![]));
        let lines = shown(&m);
        assert!(lines.contains(
            &"     no detached sessions on devbox (1 attached: . shows them)".to_string()
        ));
        assert!(lines.contains(&"     no sessions on nas".to_string()));
        m.feed(b".", 0);
        let lines = shown(&m);
        assert_eq!(lines[0], "acs: all sessions on every host");
        // Shown now; the cursor keeps its row (exit), as `.` always does.
        assert!(
            lines[3].starts_with("  1  devbox  busy  attached"),
            "{lines:?}"
        );
    }

    #[test]
    fn every_host_choices_name_their_host() {
        let mut m = every();
        m.set_answer(0, Answer::Sessions(vec![info("main", false)]));
        m.set_answer(
            1,
            Answer::Sessions(vec![info("main", false), info("busy", true)]),
        );
        // The same name on two hosts: the number picks the row's host.
        assert_eq!(
            m.feed(b"2", 0),
            Some(Choice::Attach {
                host: 1,
                name: "main".into(),
                force: false
            })
        );
        // n makes a new session on the cursor's host; on exit it explains.
        let mut m = every();
        m.set_answer(1, Answer::Sessions(vec![info("work", false)]));
        assert_eq!(m.feed(b"n", 0), Some(Choice::New { host: 1 }));
        m.feed(b"j", 0);
        assert_eq!(m.feed(b"n", 0), None);
        assert!(shown(&m)
            .iter()
            .any(|l| l.starts_with("n makes a new session")));
        // x asks with the host named and ends that host's session.
        m.feed(b"k", 0);
        assert_eq!(m.feed(b"x", 0), None);
        assert!(
            shown(&m)
                .iter()
                .any(|l| l.starts_with("end session 'work' on nas?")),
            "{:?}",
            shown(&m)
        );
        assert_eq!(
            m.feed(b"y", 0),
            Some(Choice::Kill {
                host: 1,
                name: "work".into()
            })
        );
        // Its answer comes back for that host only; the others keep theirs.
        m.set_sessions(1, vec![]);
        assert_eq!(m.sessions(1), &[]);
        assert_eq!(m.host_name(1), "nas");
        // Taking over asks with the host named.
        let mut m = every();
        m.set_answer(2, Answer::Sessions(vec![info("busy", true)]));
        m.feed(b".1", 0);
        assert!(shown(&m)
            .iter()
            .any(|l| l.starts_with("session 'busy' on pi is attached from alice@laptop")));
        assert_eq!(
            m.feed(b"y", 0),
            Some(Choice::Attach {
                host: 2,
                name: "busy".into(),
                force: true
            })
        );
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
