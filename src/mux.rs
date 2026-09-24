//! The ssh master acs owns (DESIGN §3, §7.1, §12 decision 10, acs-9n3).
//!
//! A full ssh handshake is six to eight round trips plus, on a hardware
//! key, a touch — paid again on every `acs <host>` and on every
//! `acs list <host>`. OpenSSH can skip all of it by opening a second
//! channel on a connection that is already up, and acs keeps one of its
//! own for the purpose: its own `ControlPath`, in its own directory, with
//! its own `ControlPersist`, never the user's.
//!
//! Three rules make that safe, and each is a line of code below:
//!
//! - **A redial never multiplexes** ([`crate::ssh::Call::Redial`]). The
//!   hazard §3 opted out of — a reconnect waiting on a dead multiplexer —
//!   belongs to the redial, and the redial still gets its own connection.
//!   A link that *was* multiplexed takes the master down with it
//!   ([`stop`]), because the master's TCP is the one that just failed.
//! - **A master must answer fast or not at all.** Joining one is supposed
//!   to cost a round trip; if the marker does not arrive within
//!   `ACS_CONTROL_FALLBACK_MS` the master is killed and the dial is made
//!   again on a connection of its own (`client::dial`).
//! - **The socket is ours alone.** A control socket is a filesystem object
//!   that grants a shell on the far end, so it lives in a `0700`
//!   directory keyed by our numeric uid, under the same checks the remote
//!   session directory gets (`session::SocketDir`, DESIGN §4.5): not a
//!   symlink, owned by us, and with no ancestor a stranger could swap.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::session::SocketDir;
use crate::sha256::{hex, Sha256};
use crate::ssh::{Call, Transport};
use crate::sys;

/// The shape of what acs asks of its master. It goes into the socket's
/// name, so a master left by an acs that asked for something else is never
/// joined — it is simply a different socket, and the old one expires on
/// its own `ControlPersist`. **Bump it whenever the options in
/// [`Transport::control_opts`] change.**
pub const FORMAT: &str = "acs-mux-1";

/// How long a master outlives its last use, in seconds
/// (`ACS_CONTROL_PERSIST`). Bounded on purpose: nothing else reaps it, so
/// this is what limits how long an authenticated connection can sit idle
/// after acs has gone — including after an `acs` that was killed outright.
pub const DEFAULT_PERSIST_SECS: u64 = 300;

/// How long a dial that joined an existing master may take to produce its
/// marker before acs gives up on the master (`ACS_CONTROL_FALLBACK_MS`).
pub const DEFAULT_FALLBACK_MS: u64 = 2_000;

const HINT: &str = "set ACS_CONTROL_DIR to a private directory";

/// Where acs keeps its control sockets, and how long a master outlives its
/// last use. `None` on a [`Transport`] means: no master, dial as §3 always
/// did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mux {
    pub dir: PathBuf,
    pub persist: u64,
}

/// `$ACS_CONTROL_DIR`, else `/tmp/acs-mux-<uid>` — keyed by the numeric
/// uid, never `$USER` or `$TMPDIR`, for the reasons DESIGN §4.1 gives the
/// remote's directory. Separate from `/tmp/acs-<uid>`: that one holds
/// session sockets and is scanned as such.
pub fn dir_path() -> PathBuf {
    match std::env::var_os("ACS_CONTROL_DIR") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(format!("/tmp/acs-mux-{}", sys::getuid())),
    }
}

/// `ACS_CONTROL_PERSIST` in seconds; `0` turns multiplexing off entirely.
pub fn persist_secs() -> u64 {
    persist_from(std::env::var("ACS_CONTROL_PERSIST").ok().as_deref())
}

fn persist_from(env: Option<&str>) -> u64 {
    match env {
        Some(v) if !v.trim().is_empty() => v.trim().parse().unwrap_or(DEFAULT_PERSIST_SECS),
        _ => DEFAULT_PERSIST_SECS,
    }
}

/// How long a joined master has to produce a marker before acs stops
/// believing in it (`ACS_CONTROL_FALLBACK_MS`).
pub fn fallback_window() -> Duration {
    Duration::from_millis(
        std::env::var("ACS_CONTROL_FALLBACK_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_FALLBACK_MS),
    )
}

/// Why this transport gets no master, or `None` if nothing objects.
///
/// A port forward is the interesting one: `-L` asked of a *client* of a
/// master is opened by the **master**, which outlives the session (acs-6f5,
/// DESIGN §7.1 promises the opposite — that the listener dies with the ssh
/// child). Rather than leave a port bound after acs has exited, a session
/// that forwards anything keeps its own connection.
fn veto(tr: &Transport) -> Option<String> {
    if tr.transport_cmd.is_some() {
        return Some("the transport is --transport-cmd, not ssh".into());
    }
    if !tr.local_forwards.is_empty() {
        return Some("-L is on this session, and a forward must die with it".into());
    }
    if let Some(o) = tr.user_opts.chunks(2).find_map(|p| match p {
        [flag, v] if flag == "-o" => v.to_str().filter(|o| {
            o.trim_start()
                .split(|c: char| c == '=' || c.is_whitespace())
                .next()
                .is_some_and(|k| {
                    ["LocalForward", "RemoteForward", "DynamicForward"]
                        .iter()
                        .any(|f| k.eq_ignore_ascii_case(f))
                })
        }),
        _ => None,
    }) {
        return Some(format!("-o {o} forwards a port, which must die with it"));
    }
    None
}

/// Give `tr` an acs-owned master where that is safe, and say what was
/// decided — the caller shows it under `-v`.
///
/// Nothing here is fatal: every reason not to multiplex leaves `tr.mux`
/// unset, and the dial is then exactly the one §3 has always made.
pub fn configure(tr: &mut Transport) -> String {
    configure_at(tr, dir_path(), persist_secs())
}

/// [`configure`], with the directory and persist window given rather than
/// read from the environment.
pub fn configure_at(tr: &mut Transport, dir: PathBuf, persist: u64) -> String {
    tr.mux = None;
    if let Some(why) = veto(tr) {
        return format!("no shared ssh master: {why}");
    }
    if persist == 0 {
        return "no shared ssh master: ACS_CONTROL_PERSIST is 0".into();
    }
    if let Err(e) = SocketDir::open_at(dir.clone()) {
        return format!("no shared ssh master: {}", e.with_hint(HINT));
    }
    tr.mux = Some(Mux {
        dir: dir.clone(),
        persist,
    });
    match tr.control_path() {
        Some(p) => format!(
            "shared ssh master at {} (ControlPersist {persist}s)",
            p.display()
        ),
        None => {
            // Only the length can refuse it now, and that is worth saying
            // plainly: the fix is a shorter ACS_CONTROL_DIR.
            tr.mux = None;
            format!(
                "no shared ssh master: a control socket under {} would be longer than the platform limit of {} bytes ({HINT})",
                dir.display(),
                sys::max_socket_path()
            )
        }
    }
}

/// The socket's name: a hash of everything that decides *which connection*
/// it is — the ssh binary, the user's options, the key in use and the
/// destination — so two calls share a master only when they would have
/// authenticated the same way.
///
/// ssh's own `%C` is deliberately not used: it hashes the host, port and
/// remote user but **not** `-i` or `-J`, so two `acs` calls that named
/// different keys would share one authenticated connection.
///
/// `-L` is not in it: side calls never carry a forward (acs-6f5) and a
/// session that does carries no master at all ([`veto`]), so the same host
/// keeps one socket rather than one per forward.
pub fn socket_name(tr: &Transport) -> String {
    let mut h = Sha256::new();
    let feed = |h: &mut Sha256, b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    feed(&mut h, FORMAT.as_bytes());
    feed(&mut h, tr.ssh.as_bytes());
    for o in &tr.user_opts {
        feed(&mut h, o.as_bytes());
    }
    feed(
        &mut h,
        tr.effective_identity().unwrap_or_default().as_bytes(),
    );
    feed(&mut h, tr.destination.as_bytes());
    hex(&h.finish())[..32].to_string()
}

/// Whether a dial of this kind would *join* a master that is up now: the
/// socket is there and a master answers on it. Only then is the short
/// fallback window right — a dial that has to make the connection may
/// still wait on a password or a key touch.
pub fn joinable(tr: &Transport, call: Call) -> bool {
    tr.multiplexes(call) && tr.control_path().is_some_and(|p| p.exists()) && control(tr, "check")
}

/// Kill the master, if there is one: `ssh -O exit`. Always bounded — it
/// talks to the control socket and never to the network — and harmless
/// when the socket is stale, which is why nothing here checks first.
pub fn stop(tr: &Transport) {
    if tr.control_path().is_some_and(|p| p.exists()) {
        control(tr, "exit");
    }
}

/// `ssh -O <op>`: did it succeed?
fn control(tr: &Transport, op: &str) -> bool {
    let Some(argv) = tr.control_argv(op) else {
        return false;
    };
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(sys::spawn(&mut c).and_then(|mut ch| ch.wait()), Ok(s) if s.success())
}

/// `-o <key>=<value>` as two arguments, where the value may not be UTF-8.
pub(crate) fn opt(key: &str, value: impl AsRef<std::ffi::OsStr>) -> [OsString; 2] {
    let mut v = OsString::from(key);
    v.push("=");
    v.push(value);
    ["-o".into(), v]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn tr() -> Transport {
        let mut t = Transport::new("me@box");
        t.user_opts = ["-p", "2222"].iter().map(OsString::from).collect();
        t
    }

    /// The name must change with anything that decides how the connection
    /// authenticates, or two acs calls that named different keys would
    /// share one authenticated connection.
    #[test]
    fn the_socket_name_follows_everything_that_picks_the_connection() {
        let base = socket_name(&tr());
        assert_eq!(base.len(), 32);
        assert!(base.chars().all(|c| c.is_ascii_hexdigit()), "{base}");
        assert_eq!(socket_name(&tr()), base, "the same transport, twice");

        let mut t = tr();
        t.destination = "me@other".into();
        assert_ne!(socket_name(&t), base, "destination");

        let mut t = tr();
        t.ssh = "/opt/ssh".into();
        assert_ne!(socket_name(&t), base, "ssh binary");

        let mut t = tr();
        t.user_opts = ["-p", "2223"].iter().map(OsString::from).collect();
        assert_ne!(socket_name(&t), base, "port");

        let mut t = tr();
        t.identity_file = Some("/keys/a".into());
        let with_key = socket_name(&t);
        assert_ne!(with_key, base, "a configured key");
        t.identity_file = Some("/keys/b".into());
        assert_ne!(socket_name(&t), with_key, "another key");

        // A key the command line names replaces the configured one, so the
        // socket is the one that key would have opened either way.
        let mut cmdline = tr();
        cmdline.user_opts = ["-p", "2222", "-i", "/keys/a"]
            .iter()
            .map(OsString::from)
            .collect();
        cmdline.identity_file = Some("/keys/b".into());
        let mut plain = cmdline.clone();
        plain.identity_file = None;
        assert_eq!(socket_name(&cmdline), socket_name(&plain));
    }

    /// Length-prefixing: `-o A B` must not hash like `-o AB`.
    #[test]
    fn option_boundaries_are_part_of_the_name() {
        let mut a = tr();
        a.user_opts = ["-o", "A", "B"].iter().map(OsString::from).collect();
        let mut b = tr();
        b.user_opts = ["-o", "AB"].iter().map(OsString::from).collect();
        assert_ne!(socket_name(&a), socket_name(&b));
    }

    /// A forward belongs to the session's own ssh (DESIGN §7.1): a master
    /// would open it and keep it bound after acs exits.
    #[test]
    fn a_forward_refuses_the_master() {
        let mut t = tr();
        t.local_forwards = vec!["8080:localhost:80".into()];
        assert!(veto(&t).is_some_and(|w| w.contains("-L")));

        for o in [
            "LocalForward=8080 localhost:80",
            "remoteforward 9090 localhost:90",
            " DynamicForward=1080",
        ] {
            let mut t = tr();
            t.user_opts = vec!["-o".into(), o.into()];
            assert!(veto(&t).is_some(), "{o}");
        }
        // Something that only looks like one is not one.
        let mut t = tr();
        t.user_opts = vec!["-o".into(), "LocalForwardish=1".into()];
        assert!(veto(&t).is_none());
        let mut t = tr();
        t.user_opts = vec!["-J".into(), "LocalForward=1".into()];
        assert!(veto(&t).is_none());
    }

    /// The test hook is not ssh and has no control socket to speak of.
    #[test]
    fn the_transport_hook_never_multiplexes() {
        let mut t = tr();
        t.transport_cmd = Some("sh -c".into());
        let msg = configure_at(&mut t, PathBuf::from("/nonexistent"), 300);
        assert!(t.mux.is_none(), "{msg}");
        assert!(msg.contains("--transport-cmd"), "{msg}");
    }

    #[test]
    fn a_directory_of_our_own_gets_the_master_and_0700() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new();
        let dir = d.path().join("mux");
        let mut t = tr();
        let msg = configure_at(&mut t, dir.clone(), 300);
        assert!(t.mux.is_some(), "{msg}");
        assert!(msg.contains("ControlPersist 300s"), "{msg}");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the control socket grants a shell on the far end"
        );
        let path = t.control_path().unwrap();
        assert_eq!(path.parent().unwrap(), dir);
        assert_eq!(path.file_name().unwrap(), socket_name(&t).as_str());
    }

    /// A directory acs cannot vouch for costs the speed-up, never the
    /// connection: the transport simply keeps the dial §3 always made.
    #[test]
    fn a_directory_that_is_not_ours_costs_only_the_master() {
        let d = TempDir::new();
        let file = d.path().join("notadir");
        std::fs::write(&file, b"").unwrap();
        let mut t = tr();
        let msg = configure_at(&mut t, file, 300);
        assert!(t.mux.is_none(), "{msg}");
        assert!(msg.contains("ACS_CONTROL_DIR"), "{msg}");
        assert!(t.control_path().is_none());
        let v = t.argv(Call::Session, "R");
        assert!(v.contains(&"ControlPath=none".into()), "{v:?}");
    }

    /// A directory somebody else owns is never used, whatever its mode:
    /// the master it holds is not ours to speak to (DESIGN §4.5).
    #[test]
    fn a_directory_owned_by_somebody_else_is_refused() {
        use crate::session::{check_dir, DirError, DirFacts};
        let facts = DirFacts {
            is_symlink: false,
            is_dir: true,
            uid: sys::getuid() + 1,
            mode: 0o700,
        };
        let e = check_dir(&PathBuf::from("/tmp/acs-mux-0"), facts, sys::getuid());
        assert!(matches!(e, Err(DirError::Foreign { .. })), "{e:?}");
        assert!(e.unwrap_err().with_hint(HINT).contains("ACS_CONTROL_DIR"));
    }

    /// A socket longer than `sun_path` would be truncated by ssh into
    /// somebody else's name; acs would rather not multiplex.
    #[test]
    fn a_path_too_long_for_a_unix_socket_is_refused() {
        let d = TempDir::new();
        let deep = d.path().join("x".repeat(sys::max_socket_path()));
        let mut t = tr();
        let msg = configure_at(&mut t, deep, 300);
        assert!(t.mux.is_none(), "{msg}");
        assert!(msg.contains("platform limit"), "{msg}");
    }

    #[test]
    fn persist_zero_turns_it_off() {
        let d = TempDir::new();
        let mut t = tr();
        let msg = configure_at(&mut t, d.path().to_path_buf(), 0);
        assert!(t.mux.is_none(), "{msg}");
        assert!(msg.contains("ACS_CONTROL_PERSIST"), "{msg}");
        assert_eq!(persist_from(Some("0")), 0);
        assert_eq!(persist_from(Some(" 90 ")), 90);
        assert_eq!(persist_from(None), DEFAULT_PERSIST_SECS);
        assert_eq!(persist_from(Some("")), DEFAULT_PERSIST_SECS);
        assert_eq!(persist_from(Some("soon")), DEFAULT_PERSIST_SECS);
    }

    #[test]
    fn the_default_directory_is_keyed_by_the_numeric_uid() {
        // Nothing else in the suite sets it, so this is what a user
        // without one gets.
        assert!(std::env::var_os("ACS_CONTROL_DIR").is_none());
        let p = dir_path();
        assert_eq!(p, PathBuf::from(format!("/tmp/acs-mux-{}", sys::getuid())));
        // Never the session directory: that one is listed and pruned.
        assert_ne!(p, SocketDir::default_path());
    }
}
