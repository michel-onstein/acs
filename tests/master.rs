//! The master process (DESIGN §4.2, §5.2), driven over its unix socket.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use acs::proto::{AttachKind, Mode, Msg, Resume, Welcome};
use acs::testutil::{hello, FrameConn, TempDir};

const T: Duration = Duration::from_secs(10);

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acs"))
}

fn sock(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.sock"))
}

/// Start a master and create the session running `command`.
fn start(dir: &Path, name: &str, command: &[&str], identity: &str) -> (FrameConn, Welcome) {
    start_env(dir, name, command, identity, &[])
}

fn start_env(
    dir: &Path,
    name: &str,
    command: &[&str],
    identity: &str,
    env: &[(&str, &str)],
) -> (FrameConn, Welcome) {
    acs::master::spawn_with_env(&exe(), dir, name, env).expect("spawn master");
    let mut c = FrameConn::connect(&sock(dir, name)).unwrap();
    let mut h = hello(name, Mode::AttachOrCreate, identity);
    h.command = command.iter().map(|s| s.to_string()).collect();
    c.send(&Msg::Hello(h));
    match c.recv_control(T) {
        Some(Msg::Welcome(w)) => (c, w),
        other => panic!("expected WELCOME, got {other:?}"),
    }
}

fn attach(dir: &Path, name: &str, identity: &str, resume: Option<Resume>) -> (FrameConn, Msg) {
    let mut c = FrameConn::connect(&sock(dir, name)).unwrap();
    let mut h = hello(name, Mode::Attach, identity);
    h.resume = resume;
    c.send(&Msg::Hello(h));
    let m = c.recv_control(T).expect("reply to HELLO");
    (c, m)
}

fn wait_exit(c: &mut FrameConn) -> i32 {
    match c.recv_control(Duration::from_secs(15)) {
        Some(Msg::Exit { status }) => status,
        other => panic!("expected EXIT, got {other:?}"),
    }
}

fn wait_gone(path: &Path) {
    let deadline = Instant::now() + T;
    while path.exists() {
        assert!(Instant::now() < deadline, "{} still exists", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- lifecycle (acs-5v9.9) --------------------------------------------------

#[test]
fn creates_runs_and_reports_exit_status() {
    let t = TempDir::new();
    let (mut c, w) = start(
        t.path(),
        "s",
        &[
            "/bin/sh",
            "-c",
            "echo ready:$ACS_SESSION:$TERM; read line; echo got:$line; exit 7",
        ],
        "me@here",
    );
    assert!(w.created);
    assert_eq!(w.session, "s");
    assert_eq!(w.offset, 0);
    c.wait_output("ready:s:xterm-256color", T);
    c.send(&Msg::Input {
        seq: w.input_seq,
        bytes: b"abc\r".to_vec(),
    });
    c.wait_output("got:abc", T);
    let status = wait_exit(&mut c);
    assert_eq!(acs::sys::exit_code(status), 7);
    wait_gone(&sock(t.path(), "s"));
}

#[test]
fn concurrent_starts_produce_one_master() {
    let t = TempDir::new();
    let dir = t.path().to_path_buf();
    let results: Vec<_> = (0..6)
        .map(|_| {
            let dir = dir.clone();
            std::thread::spawn(move || acs::master::spawn(&exe(), &dir, "race"))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert!(r.to_string().contains("already has a master"), "{r}");
    }
    // The winner answers; create the session and end it.
    let mut c = FrameConn::connect(&sock(&dir, "race")).unwrap();
    let mut h = hello("race", Mode::AttachOrCreate, "me");
    h.command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    c.send(&Msg::Hello(h));
    assert!(matches!(c.recv_control(T), Some(Msg::Welcome(_))));
    assert_eq!(acs::sys::exit_code(wait_exit(&mut c)), 3);
}

#[test]
fn socket_is_rebound_after_deletion() {
    let t = TempDir::new();
    let (mut c, w) = start_env(
        t.path(),
        "rb",
        &["/bin/sh", "-c", "read x; echo got:$x"],
        "me",
        &[("ACS_MASTER_REBIND_MS", "100")],
    );
    std::fs::remove_file(sock(t.path(), "rb")).unwrap();
    let deadline = Instant::now() + T;
    while !sock(t.path(), "rb").exists() {
        assert!(Instant::now() < deadline, "socket never re-bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The new socket reaches the same session.
    let (mut c2, m) = attach(t.path(), "rb", "me", None);
    assert!(
        matches!(m, Msg::Welcome(ref w2) if w2.instance == w.instance),
        "{m:?}"
    );
    assert!(matches!(c.recv_control(T), Some(Msg::Takeover)));
    c2.send(&Msg::Input {
        seq: 0,
        bytes: b"x\r".to_vec(),
    });
    c2.wait_output("got:x", T);
}

#[test]
fn attach_to_a_session_that_is_not_there() {
    let t = TempDir::new();
    acs::master::spawn(&exe(), t.path(), "empty").unwrap();
    let (_c, m) = attach(t.path(), "empty", "me", None);
    assert!(
        matches!(
            m,
            Msg::Error {
                code: acs::proto::err::NO_SESSION,
                ..
            }
        ),
        "{m:?}"
    );
}

#[test]
fn master_rejects_a_mismatched_protocol() {
    let t = TempDir::new();
    let (_c, _) = start(t.path(), "pm", &["/bin/sh", "-c", "sleep 30"], "me");
    let mut c = FrameConn::connect(&sock(t.path(), "pm")).unwrap();
    let mut h = hello("pm", Mode::Attach, "me");
    h.proto = 999;
    c.send(&Msg::Hello(h));
    match c.recv_control(T) {
        Some(Msg::Error { code, message }) => {
            assert_eq!(code, acs::proto::err::PROTO_MISMATCH);
            assert!(message.contains("finish or kill"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

// ---- session protocol (acs-5v9.10) ------------------------------------------

/// Numbers of the `n<i>` lines in `out`, in order.
fn numbers(out: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(out)
        .split(['\r', '\n'])
        .filter_map(|l| l.strip_prefix('n').and_then(|n| n.parse().ok()))
        .collect()
}

const TICKER: &str = "i=0; while true; do i=$((i+1)); echo n$i; sleep 0.02; done";

#[test]
fn resume_replays_exactly_the_missed_bytes() {
    let t = TempDir::new();
    let (mut a, w) = start(t.path(), "r", &["/bin/sh", "-c", TICKER], "me");
    a.wait_output("n3\r\n", T);
    let seen = a.next_offset.unwrap();
    let before = a.output.clone();
    drop(a); // the link drops: no DETACH
    std::thread::sleep(Duration::from_millis(300));

    let (mut b, m) = attach(
        t.path(),
        "r",
        "me",
        Some(Resume {
            instance: w.instance,
            offset: seen,
        }),
    );
    match m {
        Msg::Welcome(x) => {
            assert_eq!(x.kind, AttachKind::Resumed);
            assert_eq!(x.offset, seen);
            assert!(!x.created);
        }
        other => panic!("{other:?}"),
    }
    b.next_offset = Some(seen);
    let target = format!("n{}\r\n", numbers(&before).last().unwrap() + 20);
    b.wait_output(&target, T);
    let mut all = before;
    all.extend_from_slice(&b.output);
    let n = numbers(&all);
    assert_eq!(
        n,
        (1..=n.len() as u32).collect::<Vec<_>>(),
        "lines lost or repeated"
    );
}

#[test]
fn overwritten_offset_is_a_gap_and_other_instance_is_fresh() {
    let t = TempDir::new();
    let (a, w) = start_env(
        t.path(),
        "g",
        &[
            "/bin/sh",
            "-c",
            "head -c 100000 /dev/zero | tr '\\0' x; echo; echo done; sleep 30",
        ],
        "me",
        &[("ACS_RING", "4096")],
    );
    drop(a);
    std::thread::sleep(Duration::from_millis(500));
    let (_b, m) = attach(
        t.path(),
        "g",
        "me",
        Some(Resume {
            instance: w.instance,
            offset: 0,
        }),
    );
    match m {
        Msg::Welcome(x) => {
            assert_eq!(x.kind, AttachKind::Gap);
            // Offset 0 fell out of the 4 KiB ring: more than that was written.
            assert!(x.offset > 4096, "{}", x.offset);
        }
        other => panic!("{other:?}"),
    }
    let (_c, m) = attach(
        t.path(),
        "g",
        "me",
        Some(Resume {
            instance: w.instance ^ 1,
            offset: 0,
        }),
    );
    assert!(
        matches!(m, Msg::Welcome(ref x) if x.kind == AttachKind::Fresh),
        "{m:?}"
    );
}

#[test]
fn takeover_same_identity_is_silent_other_identity_is_busy() {
    let t = TempDir::new();
    let (mut a, _) = start(t.path(), "tk", &["/bin/sh", "-c", "sleep 30"], "me@one");
    a.send(&Msg::Ping(42));
    assert!(matches!(a.recv(T), Some(Msg::Pong(42))));

    // Someone else: BUSY, and the first client is untouched.
    let (mut b, m) = attach(t.path(), "tk", "alice@two", None);
    assert!(
        matches!(m, Msg::Busy { ref identity, .. } if identity == "me@one"),
        "{m:?}"
    );
    a.send(&Msg::Ping(1));
    assert!(matches!(a.recv(T), Some(Msg::Pong(1))));

    // Forcing takes over.
    let mut h = hello("tk", Mode::Attach, "alice@two");
    h.force = true;
    b.send(&Msg::Hello(h));
    assert!(matches!(b.recv_control(T), Some(Msg::Welcome(_))));
    assert!(matches!(a.recv_control(T), Some(Msg::Takeover)));

    // The same identity takes over silently.
    let (_c, m) = attach(t.path(), "tk", "alice@two", None);
    assert!(matches!(m, Msg::Welcome(_)), "{m:?}");
    assert!(matches!(b.recv_control(T), Some(Msg::Takeover)));
}

#[test]
fn kill_escalates_to_sigkill_for_a_group_ignoring_sighup() {
    let t = TempDir::new();
    let (mut a, _) = start(
        t.path(),
        "k",
        &["/bin/sh", "-c", "trap '' HUP; echo armed; sleep 1000"],
        "me",
    );
    a.wait_output("armed", T);
    let t0 = Instant::now();
    a.send(&Msg::Kill);
    let status = wait_exit(&mut a);
    assert!(
        t0.elapsed() >= Duration::from_millis(2500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(acs::sys::exit_code(status), 128 + libc::SIGKILL as u8);
    wait_gone(&sock(t.path(), "k"));
}

#[test]
fn kill_ends_a_normal_session_quickly() {
    let t = TempDir::new();
    let (mut a, _) = start(
        t.path(),
        "k2",
        &["/bin/sh", "-c", "echo armed; sleep 1000"],
        "me",
    );
    a.wait_output("armed", T);
    let t0 = Instant::now();
    a.send(&Msg::Kill);
    let status = wait_exit(&mut a);
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    assert_eq!(acs::sys::exit_code(status), 128 + libc::SIGHUP as u8);
}

#[test]
fn backpressure_engages_for_a_stalled_client() {
    let t = TempDir::new();
    let total = 3_000_000;
    let cmd = format!("head -c {total} /dev/zero | tr '\\0' x; printf END; sleep 30");
    let (mut a, _) = start_env(
        t.path(),
        "bp",
        &["/bin/sh", "-c", &cmd],
        "me",
        &[("ACS_RING", "65536")],
    );
    // Do not read for a while: the master must stop reading the pty rather
    // than overwrite what we have not received.
    a.set_paused(true);
    std::thread::sleep(Duration::from_secs(1));
    a.set_paused(false);
    a.wait_output("END", Duration::from_secs(30));
    let xs = a.output.iter().filter(|&&b| b == b'x').count();
    assert_eq!(xs, total, "output lost despite backpressure");
}

#[test]
fn no_backpressure_while_detached() {
    let t = TempDir::new();
    let marker = t.path().join("done");
    let cmd = format!(
        "head -c 3000000 /dev/zero | tr '\\0' x; touch {}; sleep 30",
        marker.display()
    );
    let (mut a, _) = start_env(
        t.path(),
        "nb",
        &["/bin/sh", "-c", &cmd],
        "me",
        &[("ACS_RING", "65536")],
    );
    a.send(&Msg::Detach);
    let deadline = Instant::now() + T;
    while !marker.exists() {
        assert!(Instant::now() < deadline, "program blocked while detached");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn resize_reaches_the_program() {
    let t = TempDir::new();
    let (mut a, w) = start(
        t.path(),
        "rs",
        &["/bin/sh", "-c", "read x; stty size"],
        "me",
    );
    a.send(&Msg::Resize(acs::proto::WinSize {
        cols: 100,
        rows: 30,
        xpixel: 0,
        ypixel: 0,
    }));
    a.send(&Msg::Input {
        seq: w.input_seq,
        bytes: b"\r".to_vec(),
    });
    a.wait_output("30 100", T);
}

#[test]
fn fresh_attach_forces_a_redraw() {
    let t = TempDir::new();
    let (mut a, _) = start(
        t.path(),
        "fr",
        &[
            "/bin/sh",
            "-c",
            "trap 'echo WINCH' WINCH; echo ready; while :; do sleep 0.1; done",
        ],
        "me",
    );
    a.wait_output("ready", T);
    drop(a);
    // Same size as before: only the forced SIGWINCH can cause a redraw.
    let (mut b, m) = attach(t.path(), "fr", "me", None);
    assert!(
        matches!(m, Msg::Welcome(ref x) if x.kind == AttachKind::Fresh),
        "{m:?}"
    );
    b.wait_output("WINCH", T);
}

#[test]
fn duplicate_input_after_a_resend_is_written_once() {
    let t = TempDir::new();
    let (mut a, w) = start(
        t.path(),
        "in",
        &["/bin/sh", "-c", "read x; echo got:$x"],
        "me",
    );
    let s = w.input_seq;
    a.send(&Msg::Input {
        seq: s,
        bytes: b"ab".to_vec(),
    });
    // A reconnecting client resends overlapping bytes.
    a.send(&Msg::Input {
        seq: s,
        bytes: b"abc\r".to_vec(),
    });
    a.wait_output("got:abc", T);
    match a.recv_control(T) {
        Some(Msg::Exit { .. }) => {}
        other => panic!("{other:?}"),
    }
}
