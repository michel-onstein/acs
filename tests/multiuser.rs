//! Isolation between Unix accounts (acs-5v9.19, DESIGN §4.5). These need
//! root and two users, `alice` and `bob`: they run in a Linux container via
//! `scripts/test_linux.sh`, which sets `ACS_MULTIUSER_TEST=1`. Elsewhere
//! they are skipped.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use acs::proto::{Marker, Mode, Msg};
use acs::testutil::{hello, FrameConn};

/// As `common::T`, which this file cannot reach: generous, because the
/// suite runs its targets in parallel (acs-kip).
const T: Duration = Duration::from_secs(30);
const PEER_ENV: &str = "ACS_PEER_HELPER_SOCKET";

fn enabled() -> bool {
    std::env::var("ACS_MULTIUSER_TEST").is_ok() && acs::sys::getuid() == 0
}

struct User {
    uid: u32,
    gid: u32,
    home: PathBuf,
}

fn user(name: &str) -> User {
    let (uid, gid, home) = acs::sys::user_by_name(name).unwrap_or_else(|| panic!("no user {name}"));
    User { uid, gid, home }
}

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_acs")
}

/// A command run as `u`, with a clean environment for that user.
fn as_user(u: &User, program: &str) -> Command {
    let mut c = Command::new(program);
    c.uid(u.uid)
        .gid(u.gid)
        .env_clear()
        .env("HOME", &u.home)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir(&u.home);
    c
}

fn dir_of(u: &User) -> PathBuf {
    PathBuf::from(format!("/tmp/acs-{}", u.uid))
}

fn reset(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_dir_all(p);
}

/// Start `acs _proxy` as `u` creating `session` (running `sleep`).
fn start_session(
    u: &User,
    session: &str,
    extra_env: &[(&str, &str)],
) -> (FrameConn, std::process::Child) {
    let mut cmd = as_user(u, exe());
    cmd.args(["_proxy", "--session", session, "--mode", "attach-or-create"]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut c = FrameConn::from_io(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    assert!(matches!(c.expect_marker(T), Some(Marker::Ready { .. })));
    let mut h = hello(session, Mode::AttachOrCreate, "test");
    h.command = vec!["/bin/sleep".into(), "100".into()];
    c.send(&Msg::Hello(h));
    assert!(
        matches!(c.recv_control(T), Some(Msg::Welcome(_))),
        "no WELCOME"
    );
    (c, child)
}

#[test]
fn a_squatted_socket_directory_is_refused() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    let d = dir_of(&bob);
    reset(&d);
    assert!(as_user(&alice, "/bin/mkdir")
        .arg("-m")
        .arg("0700")
        .arg(&d)
        .status()
        .unwrap()
        .success());
    let out = as_user(&bob, exe())
        .args(["_proxy", "--list"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is owned by alice"), "{err}");
    assert!(err.contains("ACS_SOCKET_DIR"), "{err}");
    reset(&d);
}

#[test]
fn a_symlinked_socket_directory_is_refused() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    let d = dir_of(&bob);
    reset(&d);
    let target = alice.home.join("trap");
    let _ = std::fs::create_dir_all(&target);
    assert!(as_user(&alice, "/bin/ln")
        .arg("-s")
        .arg(&target)
        .arg(&d)
        .status()
        .unwrap()
        .success());
    let out = as_user(&bob, exe())
        .args(["_proxy", "--list"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is a symlink"), "{err}");
    reset(&d);
}

/// Child side of the peer test: connect to a socket and report whether the
/// master answered a HELLO. Does nothing unless re-run by the parent.
#[test]
fn peer_helper() {
    let Ok(sock) = std::env::var(PEER_ENV) else {
        return;
    };
    let mut c = match FrameConn::connect(Path::new(&sock)) {
        Ok(c) => c,
        Err(e) => {
            println!("RESULT connect-failed {e}");
            return;
        }
    };
    // The master may drop us before the HELLO is even written.
    if c.try_send(&Msg::Hello(hello("s", Mode::Attach, "intruder")))
        .is_err()
    {
        println!("RESULT closed");
        return;
    }
    match c.recv_control(Duration::from_secs(5)) {
        Some(Msg::Welcome(_)) => println!("RESULT welcomed"),
        Some(other) => println!("RESULT other {other:?}"),
        None => println!("RESULT closed"),
    }
}

#[test]
fn another_uid_is_dropped_by_the_master() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    let dir = PathBuf::from("/tmp/acs-peer-test");
    reset(&dir);
    let dir_s = dir.to_str().unwrap();
    let (_c, _child) = start_session(&bob, "s", &[("ACS_SOCKET_DIR", dir_s)]);
    // Loosen the filesystem permissions so only the master's own check
    // stands between alice and bob's session.
    Command::new("/bin/chmod")
        .args(["0755", dir_s])
        .status()
        .unwrap();
    let sock = dir.join("s.sock");
    Command::new("/bin/chmod")
        .arg("0777")
        .arg(&sock)
        .status()
        .unwrap();

    let out = as_user(&alice, std::env::current_exe().unwrap().to_str().unwrap())
        .args(["--exact", "peer_helper", "--nocapture", "--test-threads=1"])
        .env(PEER_ENV, &sock)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // libtest prints "test peer_helper ... " on the same line.
    let result = text
        .find("RESULT ")
        .map(|i| text[i..].lines().next().unwrap_or(""))
        .unwrap_or("RESULT none");
    assert_eq!(
        result,
        "RESULT closed",
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Bob himself is answered (BUSY: his session is attached from another
    // identity) rather than dropped.
    let out = as_user(&bob, std::env::current_exe().unwrap().to_str().unwrap())
        .args(["--exact", "peer_helper", "--nocapture", "--test-threads=1"])
        .env(PEER_ENV, &sock)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("RESULT welcomed") || text.contains("RESULT other Busy"),
        "{text}"
    );
    reset(&dir);
}

#[test]
fn two_users_main_sessions_are_independent() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    reset(&dir_of(&alice));
    reset(&dir_of(&bob));
    let (_a, _ac) = start_session(&alice, "main", &[]);
    let (_b, _bc) = start_session(&bob, "main", &[]);
    assert!(dir_of(&alice).join("main.sock").exists());
    assert!(dir_of(&bob).join("main.sock").exists());
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(dir_of(&alice)).unwrap().uid(), alice.uid);
    assert_eq!(
        std::fs::metadata(dir_of(&bob)).unwrap().mode() & 0o777,
        0o700
    );
    // Each sees only their own session.
    let out = as_user(&alice, exe())
        .args(["_proxy", "--list"])
        .output()
        .unwrap();
    let mut c = FrameConn::from_io(std::io::Cursor::new(out.stdout), std::io::sink());
    assert!(matches!(c.expect_marker(T), Some(Marker::Ready { .. })));
    let mut names = Vec::new();
    while let Some(Msg::StatusReply(s)) = c.recv(Duration::from_secs(2)) {
        names.push(s.name);
    }
    assert_eq!(names, ["main"]);
}
