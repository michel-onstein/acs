//! The weekly check for a newer release (acs-fh5, DESIGN §7.6), against a
//! fake release server; `acs list` stands for any client start.
//!
//! Every start is *witnessed* (acs-grpq): the client gets the write end of a
//! pipe as an extra descriptor, which the background `_update-check` it may
//! start inherits through `sh`. The pipe reaches end of file only once the
//! client and every check it set off have exited, so "no check was made" is
//! asserted after the check had every chance to run, not after a sleep it
//! may have outlasted.

mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use acs::testutil::TempDir;
use common::*;

struct Setup {
    remote: Remote,
    state: TempDir,
    server: ReleaseServer,
}

impl Setup {
    fn new() -> Setup {
        Setup {
            remote: Remote::installed(),
            state: TempDir::new(),
            server: ReleaseServer::start(),
        }
    }

    fn file(&self) -> PathBuf {
        self.state.path().join("acs/update-check")
    }

    fn write_state(&self, text: &str) {
        std::fs::create_dir_all(self.file().parent().unwrap()).unwrap();
        std::fs::write(self.file(), text).unwrap();
    }

    fn read_state(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }

    /// `acs list devbox` with the check turned on; returns stderr once the
    /// client and any background check it started have exited.
    fn start(&self, extra: &[(&str, &str)]) -> String {
        self.start_as(&exe(), extra)
    }

    /// [`Setup::start`] running the copy of acs at `acs`.
    fn start_as(&self, acs: &Path, extra: &[(&str, &str)]) -> String {
        let mut c = acs_cmd_as(acs);
        c.args([
            "list",
            "--transport-cmd",
            &self.remote.transport(),
            "devbox",
        ])
        .env("ACS_NO_UPDATE_CHECK", "")
        .env("XDG_STATE_HOME", self.state.path())
        .env("ACS_RELEASES_URL", &self.server.url)
        // The server signs with a key of its own; acs takes it only
        // because this is not the default channel (acs-o9v).
        .env("ACS_RELEASE_KEY", self.server.release_key());
        for (k, v) in extra {
            c.env(k, v);
        }
        let (witness, w) = acs::sys::pipe().unwrap();
        let wfd = w.as_raw_fd();
        // SAFETY: async-signal-safe calls only.
        unsafe {
            c.pre_exec(move || {
                // dup2 leaves the copy without close-on-exec, so it is
                // inherited by everything the client starts.
                if wfd == WITNESS_FD {
                    let flags = libc::fcntl(wfd, libc::F_GETFD);
                    libc::fcntl(wfd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                } else if libc::dup2(wfd, WITNESS_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = output_of(&mut c);
        drop(w);
        assert!(out.status.success(), "{out:?}");
        wait_eof(&witness);
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn wait_state(&self, want: &str) {
        wait_until(
            T,
            || self.read_state().contains(want),
            || format!("state never had {want:?}: {:?}", self.read_state()),
        );
    }
}

/// Where a start's witness pipe sits in the client.
const WITNESS_FD: i32 = 9;

/// Wait until every holder of the write end of `r` has exited.
fn wait_eof(r: &OwnedFd) {
    let deadline = Instant::now() + T;
    let mut buf = [0u8; 64];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "a background check outlived {T:?}");
        let mut p = [acs::sys::pollfd(r.as_raw_fd(), libc::POLLIN)];
        if acs::sys::poll_retry(&mut p, left.as_millis() as i32).unwrap() > 0
            && acs::sys::read(r.as_raw_fd(), &mut buf).unwrap() == 0
        {
            return;
        }
    }
}

fn now() -> u64 {
    acs::sys::unix_now()
}

fn message(v: &str) -> String {
    format!(
        "acs: acs {v} is available (you have {}) — run: acs upgrade\n",
        acs::VERSION
    )
}

#[test]
fn a_newer_release_is_shown_once() {
    let s = Setup::new();
    s.write_state(&format!("checked={}\nlatest=9.9.9\n", now()));
    assert_eq!(s.start(&[]), message("9.9.9"));
    assert!(s.read_state().contains("shown=9.9.9"), "{}", s.read_state());
    assert_eq!(s.start(&[]), "");
    // Checked just now: nothing was asked.
    assert!(s.server.hits().is_empty(), "{:?}", s.server.hits());
}

#[test]
fn a_brewed_acs_says_to_upgrade_with_brew() {
    let s = Setup::new();
    let d = TempDir::new();
    s.write_state(&format!("checked={}\nlatest=9.9.9\n", now()));
    assert_eq!(
        s.start_as(&brewed_copy(d.path()), &[]),
        format!(
            "acs: acs 9.9.9 is available (you have {}) — run: brew upgrade acs\n",
            acs::VERSION
        )
    );
}

#[test]
fn a_due_check_runs_in_the_background_and_shows_next_time() {
    let s = Setup::new();
    s.server
        .release("9.9.9", &ReleaseServer::fake_acs("9.9.9"), true, false);
    // No state yet: a check is due; this start says nothing.
    assert_eq!(s.start(&[]), "");
    // Already there, without waiting: the start's witness covered the
    // background check. Were it ever to stop doing so, this fails rather
    // than every "no check was made" below passing without a check.
    assert!(
        s.read_state().contains("latest=9.9.9"),
        "the witness did not wait for the background check: {:?}",
        s.read_state()
    );
    // The signature is fetched and checked before the sums are read, on
    // the background check as much as on an upgrade (acs-o9v).
    assert_eq!(
        s.server.hits(),
        [
            "/latest/download/SHA256SUMS",
            "/latest/download/SHA256SUMS.sig"
        ]
    );
    // The next start shows it.
    assert_eq!(s.start(&[]), message("9.9.9"));
}

#[test]
fn a_check_is_made_once_a_week() {
    let s = Setup::new();
    s.server
        .release("9.9.9", &ReleaseServer::fake_acs("9.9.9"), true, false);
    let recent = format!("checked={}\n", now() - 6 * 86_400);
    s.write_state(&recent);
    s.start(&[]);
    assert!(s.server.hits().is_empty(), "{:?}", s.server.hits());
    assert_eq!(s.read_state(), recent);

    s.write_state(&format!("checked={}\n", now() - 7 * 86_400 - 60));
    s.start(&[]);
    s.wait_state("latest=9.9.9");
    let checked: u64 = s
        .read_state()
        .lines()
        .find_map(|l| l.strip_prefix("checked="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(now() - checked < 60, "{checked}");
}

#[test]
fn it_can_be_turned_off() {
    let s = Setup::new();
    s.server
        .release("9.9.9", &ReleaseServer::fake_acs("9.9.9"), true, false);
    s.write_state("checked=0\nlatest=9.9.9\n");
    assert_eq!(s.start(&[("ACS_NO_UPDATE_CHECK", "1")]), "");

    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "update_check: false\n");
    let (k, v) = &env[0];
    assert_eq!(s.start(&[(k.as_str(), v.as_str())]), "");

    assert!(s.server.hits().is_empty(), "{:?}", s.server.hits());
    assert_eq!(s.read_state(), "checked=0\nlatest=9.9.9\n");
}

#[test]
fn offline_is_silent() {
    let s = Setup::new();
    // Nothing listens on port 1.
    assert_eq!(s.start(&[("ACS_RELEASES_URL", "http://127.0.0.1:1")]), "");
    // The attempt is recorded (so it is not retried on every start), and no
    // version was learned (the witnessed start saw the attempt finish).
    s.wait_state("checked=");
    let st = s.read_state();
    assert!(st.starts_with("checked="), "{st}");
    assert!(!st.contains("latest"), "{st}");
}

#[test]
fn the_background_check_on_its_own() {
    let s = Setup::new();
    s.server
        .release("9.9.9", &ReleaseServer::fake_acs("9.9.9"), true, false);
    s.write_state("checked=5\nshown=1.0.0\n");
    let out = acs_cmd()
        .arg("_update-check")
        .env("XDG_STATE_HOME", s.state.path())
        .env("ACS_RELEASES_URL", &s.server.url)
        .env("ACS_RELEASE_KEY", s.server.release_key())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert_eq!(s.read_state(), "checked=5\nlatest=9.9.9\nshown=1.0.0\n");
}

#[test]
fn the_state_file_defaults_under_home() {
    let s = Setup::new();
    let home = TempDir::new();
    let mut c = acs_cmd();
    c.args(["list", "--transport-cmd", &s.remote.transport(), "devbox"])
        .env("ACS_NO_UPDATE_CHECK", "")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", home.path())
        .env("ACS_RELEASES_URL", "http://127.0.0.1:1");
    assert!(c.output().unwrap().status.success());
    let file: &Path = &home.path().join(".local/state/acs/update-check");
    assert!(file.exists(), "no {}", file.display());
}

/// acs-2zj: a channel serving an old but genuine SHA256SUMS for ever keeps
/// every check agreeing there is nothing newer, so the upgrade message
/// never comes and the user sits on a version with a known hole believing
/// they are current. Going backwards is said out loud instead.
#[test]
fn a_release_channel_that_goes_backwards_is_reported() {
    let s = Setup::new();
    // The state already knows about a much newer release.
    s.write_state(&format!("checked={}\nlatest=9.9.9\n", now() - 8 * 86_400));
    // The channel now offers something far older.
    s.server.release("0.0.1", b"old", true, false);

    // The check runs in the background and notices.
    let _ = s.start(&[]);
    s.wait_state("regressed=0.0.1");

    // The next start says so, once, and still knows the newer version.
    let err = s.start(&[]);
    assert!(err.contains("older than"), "no warning: {err}");
    assert!(err.contains("0.0.1"), "{err}");
    assert!(err.contains("9.9.9"), "{err}");
    let again = s.start(&[]);
    assert!(!again.contains("older than"), "warned twice: {again}");
    assert!(
        s.read_state().contains("latest=9.9.9"),
        "{}",
        s.read_state()
    );
}
