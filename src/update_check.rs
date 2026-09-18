//! A weekly check for a newer release (DESIGN §7.6).
//!
//! Only the local client checks, and never on its way to a connection: when
//! the last check is a week old it starts `acs _update-check` in the
//! background (curl with a 3 s limit, `release.rs`) and goes on; that process
//! records the latest version in the state file, and a later start shows
//! `acs X is available (you have Y) — run: acs upgrade` on stderr before
//! connecting — once per new version; `brew upgrade acs` for a Homebrew
//! install (`upgrade::brewed`). Offline or rate-limited, nothing is said.
//!
//! State: `$XDG_STATE_HOME/acs/update-check` (default
//! `~/.local/state/acs/update-check`), `key=value` lines. Turned off with
//! `ACS_NO_UPDATE_CHECK=1` or `update_check: false` in the configuration.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::config::Config;
use crate::release;

/// How often to ask.
pub const INTERVAL_SECS: u64 = 7 * 86_400;

/// How long the background check may take.
const TIMEOUT_SECS: u32 = 3;

/// What the state file remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// When a check was last started (unix seconds).
    pub checked: u64,
    /// The newest release the last successful check saw.
    pub latest: Option<String>,
    /// The version the last message was about.
    pub shown: Option<String>,
}

impl State {
    /// Unknown lines and bad values are ignored: the file is a cache.
    pub fn parse(text: &str) -> State {
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            let version = || release::parse_version(v).map(|_| v.to_string());
            match k.trim() {
                "checked" => s.checked = v.parse().unwrap_or(0),
                "latest" => s.latest = version(),
                "shown" => s.shown = version(),
                _ => {}
            }
        }
        s
    }

    pub fn format(&self) -> String {
        let mut out = format!("checked={}\n", self.checked);
        if let Some(v) = &self.latest {
            out.push_str(&format!("latest={v}\n"));
        }
        if let Some(v) = &self.shown {
            out.push_str(&format!("shown={v}\n"));
        }
        out
    }

    /// Whether a new check is due at `now`: never checked, a week after the
    /// last, or at once if the clock went back.
    pub fn due(&self, now: u64) -> bool {
        self.checked == 0 || now < self.checked || now - self.checked >= INTERVAL_SECS
    }

    /// The message to show for `current`, if a newer release is known and
    /// was not shown yet; records it as shown. `upgrade` is the command that
    /// upgrades this acs (`upgrade::command`).
    pub fn take_message(&mut self, current: &str, upgrade: &str) -> Option<String> {
        let latest = self.latest.clone()?;
        let newer = release::compare(&latest, current) == Some(std::cmp::Ordering::Greater);
        if !newer || self.shown.as_deref() == Some(latest.as_str()) {
            return None;
        }
        self.shown = Some(latest.clone());
        Some(format!(
            "acs {latest} is available (you have {current}) — run: {upgrade}"
        ))
    }
}

/// `$XDG_STATE_HOME/acs/update-check`, or `~/.local/state/acs/update-check`.
pub fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("acs/update-check"))
}

fn read(path: &Path) -> State {
    std::fs::read_to_string(path)
        .map(|t| State::parse(&t))
        .unwrap_or_default()
}

/// Write the state atomically; failures are ignored (it is only a cache).
fn write(path: &Path, s: &State) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".update-check.{}", crate::sys::getpid()));
    if std::fs::write(&tmp, s.format()).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Whether checking is turned off.
pub fn disabled(config: &Config) -> bool {
    let env = std::env::var("ACS_NO_UPDATE_CHECK").unwrap_or_default();
    (!env.is_empty() && env != "0") || !config.update_check.value
}

/// Called as the client starts: show a pending message, and start a
/// background check when one is due. Never blocks on the network.
pub fn on_client_start(config: &Config) {
    if disabled(config) {
        return;
    }
    let Some(path) = state_path() else { return };
    let mut state = read(&path);
    let before = state.clone();
    if let Some(msg) = state.take_message(crate::VERSION, crate::upgrade::command()) {
        eprintln!("acs: {msg}");
    }
    let now = crate::sys::unix_now();
    let due = state.due(now);
    if due {
        // Recorded before the check, so an offline laptop asks once a week,
        // not on every start.
        state.checked = now;
    }
    if state != before {
        write(&path, &state);
    }
    // Only after writing: the check re-reads the file and adds `latest`,
    // which a later write of ours would otherwise undo.
    if due {
        start_background();
    }
}

/// Run `acs _update-check` detached: `sh` starts it in the background and
/// exits at once, so it is nobody's child to wait for.
fn start_background() {
    let Ok(exe) = crate::sys::self_exe().and_then(std::fs::canonicalize) else {
        return;
    };
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("\"$0\" _update-check </dev/null >/dev/null 2>&1 &")
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `acs _update-check`: ask for the latest release and record it.
pub fn check_main() -> ExitCode {
    let Some(path) = state_path() else {
        return ExitCode::from(1);
    };
    let tmp = std::env::temp_dir().join(format!(
        "acs-update-check.{}.{:08x}",
        crate::sys::getpid(),
        crate::sys::random_u64() as u32
    ));
    if std::fs::create_dir(&tmp).is_err() {
        return ExitCode::from(1);
    }
    let target = release::archive_target(crate::payload::OWN_TARGET);
    let found = release::lookup(&release::releases_url(), None, &target, &tmp, TIMEOUT_SECS);
    let _ = std::fs::remove_dir_all(&tmp);
    match found {
        Ok(asset) => {
            // Re-read: a client may have written `shown` meanwhile.
            let mut state = read(&path);
            state.latest = Some(asset.version);
            if state.checked == 0 {
                state.checked = crate::sys::unix_now();
            }
            write(&path, &state);
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::from(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_tolerates_garbage() {
        let s = State {
            checked: 1_700_000_000,
            latest: Some("0.3.0".into()),
            shown: Some("0.2.5".into()),
        };
        assert_eq!(State::parse(&s.format()), s);
        assert_eq!(
            State::parse("checked=soon\nlatest=newest\nnoise\nshown=0.1.0\n"),
            State {
                checked: 0,
                latest: None,
                shown: Some("0.1.0".into())
            }
        );
        assert_eq!(State::parse(""), State::default());
    }

    #[test]
    fn a_check_is_due_once_a_week() {
        let s = State {
            checked: 1_000_000,
            ..State::default()
        };
        assert!(!s.due(1_000_000));
        assert!(!s.due(1_000_000 + INTERVAL_SECS - 1));
        assert!(s.due(1_000_000 + INTERVAL_SECS));
        // A clock that went back does not silence it for years.
        assert!(s.due(999_999));
        assert!(State::default().due(1));
    }

    #[test]
    fn a_newer_release_is_shown_once() {
        let mut s = State {
            latest: Some("0.3.0".into()),
            ..State::default()
        };
        assert_eq!(
            s.take_message("0.2.0", "acs upgrade").as_deref(),
            Some("acs 0.3.0 is available (you have 0.2.0) — run: acs upgrade")
        );
        assert_eq!(s.shown.as_deref(), Some("0.3.0"));
        assert_eq!(s.take_message("0.2.0", "acs upgrade"), None);
        // A newer one again is news again; a brewed acs says to use brew.
        s.latest = Some("0.4.0".into());
        assert_eq!(
            s.take_message("0.2.0", "brew upgrade acs").as_deref(),
            Some("acs 0.4.0 is available (you have 0.2.0) — run: brew upgrade acs")
        );
    }

    #[test]
    fn nothing_to_say_when_current_or_ahead() {
        for current in ["0.3.0", "0.4.0"] {
            let mut s = State {
                latest: Some("0.3.0".into()),
                ..State::default()
            };
            assert_eq!(s.take_message(current, "acs upgrade"), None, "{current}");
            assert_eq!(s.shown, None);
        }
        assert_eq!(State::default().take_message("0.1.0", "acs upgrade"), None);
    }
}
