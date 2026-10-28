//! The progress meter on stderr (ACSCP_DESIGN §4): one line per file —
//! name, percent, size, rate, time left — redrawn in place while it goes
//! and left standing when it is done, as `scp` draws it. Shown only when
//! stderr is a terminal and `-q` is not given.

use std::io::Write;
use std::time::{Duration, Instant};

/// How often the line is redrawn at most.
const EVERY: Duration = Duration::from_millis(200);

pub struct Progress {
    width: usize,
    name: String,
    size: u64,
    done: u64,
    started: Instant,
    drawn: Option<Instant>,
}

/// `n` bytes as `scp` shows them: `512B`, `12KB`, `3.4MB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    match v < 10.0 {
        true => format!("{v:.1}{}", UNITS[u]),
        false => format!("{v:.0}{}", UNITS[u]),
    }
}

/// `secs` as `mm:ss`, or `hh:mm:ss` from an hour on.
pub fn clock(secs: u64) -> String {
    match secs < 3600 {
        true => format!("{:02}:{:02}", secs / 60, secs % 60),
        false => format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60),
    }
}

/// The meter's line for `name` at `done` of `size` bytes after
/// `elapsed`, fitted to `width` columns: the name is cut first.
pub fn line(
    name: &str,
    done: u64,
    size: u64,
    elapsed: Duration,
    finished: bool,
    width: usize,
) -> String {
    let pct = match size {
        0 => 100,
        _ => (done.min(size) as u128 * 100 / size as u128) as u64,
    };
    let secs = elapsed.as_secs_f64();
    let rate = match secs > 0.0 {
        true => (done as f64 / secs) as u64,
        false => 0,
    };
    let time = match (finished, rate) {
        (true, _) => clock(elapsed.as_secs()),
        (false, 0) => "--:-- ETA".into(),
        (false, r) => format!("{} ETA", clock(size.saturating_sub(done) / r)),
    };
    let tail = format!(
        " {pct:3}% {:>7} {:>7}/s {:>9}",
        bytes(done),
        bytes(rate),
        time
    );
    let room = width.saturating_sub(tail.chars().count() + 1).max(1);
    let shown = crate::safe::display(name);
    let name: String = match shown.chars().count() > room {
        true => shown.chars().take(room).collect(),
        false => shown,
    };
    format!("{name:<room$}{tail}")
}

impl Progress {
    /// A meter, if stderr is a terminal.
    pub fn on_terminal() -> Option<Progress> {
        if !crate::sys::isatty(2) {
            return None;
        }
        let width = crate::sys::get_winsize(2)
            .map(|w| w.cols as usize)
            .ok()
            .filter(|&w| w > 0)
            .unwrap_or(80);
        Some(Progress {
            width,
            name: String::new(),
            size: 0,
            done: 0,
            started: Instant::now(),
            drawn: None,
        })
    }

    /// A file begins.
    pub fn start(&mut self, name: &str, size: u64) {
        self.name = name.to_string();
        self.size = size;
        self.done = 0;
        self.started = Instant::now();
        self.drawn = None;
    }

    /// `n` more bytes of it went.
    pub fn add(&mut self, n: u64) {
        self.done += n;
        let now = Instant::now();
        if self.drawn.map_or(true, |d| now - d >= EVERY) {
            self.drawn = Some(now);
            self.draw(false);
        }
    }

    /// The file is done: its line drawn once more, and left standing.
    pub fn finish(&mut self) {
        self.draw(true);
        let _ = std::io::stderr().write_all(b"\n");
    }

    fn draw(&self, finished: bool) {
        let l = line(
            &self.name,
            self.done,
            self.size,
            self.started.elapsed(),
            finished,
            self.width,
        );
        let _ = write!(std::io::stderr(), "\r{l}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_clocks() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(1023), "1023B");
        assert_eq!(bytes(1024), "1.0KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0MB");
        assert_eq!(bytes(300 * 1024 * 1024), "300MB");
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(61), "01:01");
        assert_eq!(clock(3661), "1:01:01");
    }

    #[test]
    fn the_line_fits_its_width_and_cuts_the_name() {
        let l = line("a.txt", 512, 1024, Duration::from_secs(1), false, 60);
        assert_eq!(l.chars().count(), 59, "{l:?}");
        assert!(l.starts_with("a.txt "), "{l:?}");
        assert!(l.contains(" 50% "), "{l:?}");
        assert!(l.contains("512B/s"), "{l:?}");
        assert!(l.ends_with("00:01 ETA"), "{l:?}");

        let long = "n".repeat(200);
        let l = line(&long, 0, 0, Duration::ZERO, true, 40);
        assert_eq!(l.chars().count(), 39, "{l:?}");
        assert!(l.contains("100%"), "{l:?}");
    }

    /// A name is a string the far end may have chosen: it cannot steer
    /// the terminal.
    #[test]
    fn the_name_is_made_safe() {
        let l = line("x\x1b[2Jy", 0, 1, Duration::ZERO, false, 80);
        assert!(!l.contains('\x1b'), "{l:?}");
        assert!(l.contains("--:-- ETA"), "{l:?}");
    }
}
