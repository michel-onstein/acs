//! The master process (DESIGN §4.2, §5.2), driven over its unix socket.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use acs::proto::{Mode, Msg, Resume, Welcome};
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
    acs::master::spawn(&exe(), dir, name).expect("spawn master");
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
    std::env::set_var("ACS_MASTER_REBIND_MS", "100");
    let t = TempDir::new();
    let (mut c, w) = start(
        t.path(),
        "rb",
        &["/bin/sh", "-c", "read x; echo got:$x"],
        "me",
    );
    std::env::remove_var("ACS_MASTER_REBIND_MS");
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
