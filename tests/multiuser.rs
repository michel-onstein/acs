//! Isolation between Unix accounts (acs-5v9.19, DESIGN §4.5). These need
//! root and two users, `alice` and `bob`: they run in a Linux container via
//! `scripts/test_linux.sh`, which sets `ACS_MULTIUSER_TEST=1`. Elsewhere
//! they are skipped.

use std::os::unix::fs::PermissionsExt;
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

/// Where the client keeps the control sockets of the ssh masters it owns
/// (acs-9n3). One per uid, like the session directory beside it.
fn mux_dir_of(u: &User) -> PathBuf {
    PathBuf::from(format!("/tmp/acs-mux-{}", u.uid))
}

/// A client that never reaches the network: the ssh it would run is
/// `/bin/false`, and `-v` makes it say what it decided about a master.
fn client_as(u: &User) -> Command {
    let mut c = as_user(u, exe());
    c.env("ACS_SSH", "/bin/false")
        .env("ACS_NO_UPDATE_CHECK", "1")
        .env("XDG_CONFIG_HOME", "/nonexistent/acs-test-config")
        .env("ACS_GLOBAL_CONFIG", "/nonexistent/acs-test-config/g.yaml");
    c
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

/// acs-9n3: a control socket is an authenticated shell on the far end, so
/// the directory holding them is one user's and the checks that guard the
/// session directory (DESIGN §4.5) guard it too — including when someone
/// else got there first.
#[test]
fn a_control_directory_is_one_users_and_a_squatted_one_is_refused() {
    if !enabled() {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let (alice, bob) = (user("alice"), user("bob"));
    let d = mux_dir_of(&bob);
    reset(&d);

    // Alice gets there first, with a mode that gives nothing away.
    assert!(as_user(&alice, "/bin/mkdir")
        .arg("-m")
        .arg("0700")
        .arg(&d)
        .status()
        .unwrap()
        .success());
    let out = client_as(&bob)
        .args(["list", "-v", "devbox"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no shared ssh master"), "{err}");
    assert!(err.contains("is owned by alice"), "{err}");
    assert!(err.contains("ACS_CONTROL_DIR"), "{err}");
    // And bob's ssh was given no path into it: the dial is the one acs
    // has always made.
    assert!(err.contains("running "), "{err}");
    assert!(!err.contains("ControlPath=/tmp/acs-mux-"), "{err}");
    reset(&d);

    // His own directory: 0700, his, and nothing alice can open.
    let out = client_as(&bob)
        .args(["list", "-v", "devbox"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!("shared ssh master at {}/", d.display())),
        "{err}"
    );
    let meta = std::fs::metadata(&d).unwrap();
    assert_eq!(meta.uid(), bob.uid);
    assert_eq!(meta.mode() & 0o777, 0o700);
    assert!(
        !as_user(&alice, "/bin/ls")
            .arg(&d)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "alice can list bob's control sockets"
    );
    // Alice's own is somewhere else entirely.
    assert_ne!(mux_dir_of(&alice), d);
    reset(&d);
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

// ---- the system-wide candidate (acs-6w9, DESIGN §8) ------------------------

/// Everything the prelude needs to judge a path, set by root.
fn plant(path: &Path, body: &str, owner: u32, mode: u32) {
    std::fs::write(path, body).unwrap();
    std::os::unix::fs::chown(path, Some(owner), Some(0)).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// acs-6w9: DESIGN §8 offers `/usr/local/lib/acs/<version>/acs` as "an
/// optional system-wide location an administrator can populate once for all
/// users" — and an administrator populates it *as root*. acs-08m's check
/// demanded the file be owned by the invoking user, so every non-root user
/// refused it and installed its own copy: the documented feature could only
/// ever serve the one account that owned it.
///
/// The rule is now "owned by you or by root", for the file and for the
/// directory holding it. This test is the one that can only run here: it
/// really is root, and alice and bob really are other users.
#[test]
fn a_root_installed_binary_is_run_by_every_user() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    let version = "9.9.9-sys";
    // Only this version's directory is made and removed: the check looks at
    // the file and the directory holding it, never at `/usr/local/lib/acs`
    // itself, so a real install there is neither needed nor disturbed.
    let dir = PathBuf::from(format!("/usr/local/lib/acs/{version}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    set_mode(&dir, 0o755);
    let binary = dir.join("acs");
    plant(&binary, "#!/bin/sh\necho RAN\n", 0, 0o755);

    let script = acs::ssh::prelude(version, &["--version"]);
    let run = |u: &User| {
        let out = as_user(u, "/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // 1. Root put it there; both users run it. This is the whole point of
    //    the location, and it did not work before acs-6w9.
    for u in [&alice, &bob] {
        let (out, err) = run(u);
        assert!(out.contains("RAN"), "uid {} refused it: {err}", u.uid);
    }

    // 2. Root's file in a directory anyone can write: refused. Ownership of
    //    the file says nothing when someone else can replace it.
    set_mode(&dir, 0o777);
    let (out, err) = run(&alice);
    assert!(
        !out.contains("RAN"),
        "a root file in a shared directory ran"
    );
    assert!(out.contains("ACS-NEED"), "no marker: {out}");
    assert!(
        err.contains(dir.to_str().unwrap()) && err.contains("is writable by others"),
        "{err}"
    );

    // 3. Group-writable is refused too (acs-08m's own case, at this path).
    set_mode(&dir, 0o775);
    let (out, err) = run(&alice);
    assert!(
        !out.contains("RAN"),
        "a root file under a 0775 directory ran"
    );
    assert!(err.contains("is writable by others"), "{err}");
    set_mode(&dir, 0o755);
    set_mode(&binary, 0o775);
    let (out, err) = run(&alice);
    assert!(!out.contains("RAN"), "a 0775 root-owned binary ran");
    assert!(err.contains("is writable by others"), "{err}");
    set_mode(&binary, 0o755);

    // 4. A third uid is still refused: "you or root", not "anyone".
    plant(&binary, "#!/bin/sh\necho RAN\n", bob.uid, 0o755);
    let (out, err) = run(&alice);
    assert!(!out.contains("RAN"), "bob's binary ran as alice");
    assert!(
        err.contains(&format!("is owned by uid {}, not by you or root", bob.uid)),
        "{err}"
    );
    // ...and it is bob's, so bob may run it.
    assert!(
        run(&bob).0.contains("RAN"),
        "bob was refused his own binary"
    );

    // 5. The directory is judged by the same rule: bob's directory holding
    //    root's file is refused.
    plant(&binary, "#!/bin/sh\necho RAN\n", 0, 0o755);
    std::os::unix::fs::chown(&dir, Some(bob.uid), Some(0)).unwrap();
    let (out, err) = run(&alice);
    assert!(!out.contains("RAN"), "root's file in bob's directory ran");
    assert!(
        err.contains(&format!("is owned by uid {}, not by you or root", bob.uid)),
        "{err}"
    );

    std::os::unix::fs::chown(&dir, Some(0), Some(0)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// acs-6w9: root may also have baked a per-user install into an image, so
/// the same rule reaches the first candidate. What it must not reach is a
/// binary belonging to a third user — which is exactly acs-08m's attack.
#[test]
fn the_home_candidate_takes_the_same_owners_and_no_others() {
    if !enabled() {
        return;
    }
    let (alice, bob) = (user("alice"), user("bob"));
    let version = "9.9.9-home";
    // As above, only this version's directory — the one the check reads —
    // is made, owned and removed.
    let dir = alice.home.join(format!(".local/share/acs/{version}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::chown(&dir, Some(alice.uid), Some(alice.gid)).unwrap();
    set_mode(&dir, 0o755);
    let binary = dir.join("acs");
    let script = acs::ssh::prelude(version, &["--version"]);
    let run = |u: &User| {
        let out = as_user(u, "/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // Root's file in alice's directory: accepted, as root's file anywhere.
    plant(&binary, "#!/bin/sh\necho RAN\n", 0, 0o755);
    let (out, err) = run(&alice);
    assert!(
        out.contains("RAN"),
        "root's file under $HOME was refused: {err}"
    );

    // Bob's, in the same place: refused. acs-08m's attack, unchanged.
    plant(&binary, "#!/bin/sh\necho RAN\n", bob.uid, 0o755);
    let (out, err) = run(&alice);
    assert!(!out.contains("RAN"), "bob's binary under alice's $HOME ran");
    assert!(out.contains("ACS-NEED"), "no marker: {out}");
    assert!(
        err.contains(&format!("is owned by uid {}, not by you or root", bob.uid)),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
