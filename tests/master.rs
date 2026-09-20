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
fn socket_directory_is_recreated_after_deletion() {
    let t = TempDir::new();
    let dir = t.path().join("s");
    let (_c, w) = start_env(
        &dir,
        "gone",
        &["/bin/sh", "-c", "sleep 30"],
        "me",
        &[("ACS_MASTER_REBIND_MS", "100")],
    );
    std::fs::remove_dir_all(&dir).unwrap();
    let deadline = Instant::now() + T;
    while !sock(&dir, "gone").exists() {
        assert!(
            Instant::now() < deadline,
            "socket directory never recreated"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    let (_c2, m) = attach(&dir, "gone", "me", None);
    assert!(
        matches!(m, Msg::Welcome(ref x) if x.instance == w.instance),
        "{m:?}"
    );
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
    a.send(&Msg::Kill {
        identity: "me".into(),
        force: true,
    });
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
    a.send(&Msg::Kill {
        identity: "me".into(),
        force: true,
    });
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

/// Fast liveness timers for the master (DESIGN §5.3).
const FAST_LIVENESS: &[(&str, &str)] = &[("ACS_PING_MS", "200"), ("ACS_DEAD_MS", "800")];

/// Regression (acs-ode): a client that stops reading and writing without
/// closing — a laptop powered off, no FIN reaching the host — is given up
/// after the dead interval, so the program is no longer held back.
#[test]
fn a_client_that_vanishes_without_closing_is_dropped() {
    let t = TempDir::new();
    let marker = t.path().join("done");
    let cmd = format!(
        "head -c 3000000 /dev/zero | tr '\\0' x; touch {}; sleep 30",
        marker.display()
    );
    let mut env = FAST_LIVENESS.to_vec();
    env.push(("ACS_RING", "65536"));
    acs::master::spawn_with_env(&exe(), t.path(), "gone", &env).unwrap();
    let mut s = std::os::unix::net::UnixStream::connect(sock(t.path(), "gone")).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    let mut h = hello("gone", Mode::AttachOrCreate, "me");
    h.command = vec!["/bin/sh".into(), "-c".into(), cmd];
    std::io::Write::write_all(&mut s, &Msg::Hello(h).to_bytes()).unwrap();
    // Never read, never write, never close.
    let deadline = Instant::now() + T;
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "program blocked by a vanished client"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The master hung up on it.
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match std::io::Read::read(&mut s, &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
}

/// The master pings a silent client (acs-ode), and one that answers stays
/// attached past the dead interval.
#[test]
fn the_master_pings_and_keeps_a_client_that_answers() {
    let t = TempDir::new();
    let (mut a, w) = start_env(
        t.path(),
        "ping",
        &["/bin/sh", "-c", "while read l; do echo got:$l; done"],
        "me",
        FAST_LIVENESS,
    );
    // FrameConn answers each PING; wait well past the dead interval.
    let _ = a.recv_control(Duration::from_millis(2000));
    a.send(&Msg::Input {
        seq: w.input_seq,
        bytes: b"still\r".to_vec(),
    });
    a.wait_output("got:still", T);
}

/// The master sends PING to an attached client that says nothing (acs-ode).
#[test]
fn the_master_pings_a_silent_client() {
    let t = TempDir::new();
    acs::master::spawn_with_env(&exe(), t.path(), "p", FAST_LIVENESS).unwrap();
    let mut s = std::os::unix::net::UnixStream::connect(sock(t.path(), "p")).unwrap();
    let mut h = hello("p", Mode::AttachOrCreate, "me");
    h.command = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
    std::io::Write::write_all(&mut s, &Msg::Hello(h).to_bytes()).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    let mut dec = acs::proto::Decoder::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match dec.next_msg().unwrap() {
            Some(Msg::Ping(_)) => return,
            Some(_) => continue,
            None => {}
        }
        let n = std::io::Read::read(&mut s, &mut buf).expect("a PING in time");
        assert!(n > 0, "closed before a PING");
        dec.push(&buf[..n]);
    }
}

/// Regression (acs-evm): an ACK means the pty took the bytes. A program in
/// raw mode that never reads fills its terminal's input buffer, and the
/// master acknowledges only what the pty took — it used to acknowledge
/// input the moment it queued it, and a queued write that is then dropped
/// (the terminal hangs up) would be lost for good, since the client never
/// sends acknowledged input again.
#[test]
fn input_is_acked_only_as_fast_as_the_pty_takes_it() {
    let t = TempDir::new();
    let (mut c, w) = start(
        t.path(),
        "ack",
        // Says it is ready, then closes every descriptor of its terminal.
        &[
            "/bin/sh",
            "-c",
            "stty -echo -icanon min 1 time 0; echo ready; sleep 5",
        ],
        "me",
    );
    c.wait_output("ready", T);
    let n: usize = 200_000;
    let end = w.input_seq + n as u64;
    c.send(&Msg::Input {
        seq: w.input_seq,
        bytes: vec![b'x'; n],
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if let Some(Msg::Ack { seq }) = c.recv(Duration::from_millis(200)) {
            assert!(seq < end, "acked {seq}: input the pty never took");
        }
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

// ---- bug hunt 2026-09-18 -----------------------------------------------------

/// Regression (acs-u3c): the child's last line, written just before it
/// exits, reaches the client — the pty is only declared drained by a quiet
/// poll after the exit was seen, not by the poll SIGCHLD cut short.
#[test]
fn the_last_output_before_exit_is_never_lost() {
    // A burst right before the exit leaves the most in flight.
    for i in 0..100 {
        let t = TempDir::new();
        let cmd = format!("head -c 30000 /dev/zero | tr '\\0' y; echo last-{i}");
        let (mut c, _) = start(t.path(), "e", &["/bin/sh", "-c", &cmd], "me");
        let status = wait_exit(&mut c);
        assert_eq!(acs::sys::exit_code(status), 0);
        let out = String::from_utf8_lossy(&c.output);
        assert!(
            out.contains(&format!("last-{i}")),
            "run {i}: output ends {:?}",
            &out[out.len().saturating_sub(40)..]
        );
    }
}

/// Regression (acs-gkl): while a stalled client has the ring full, a pty
/// that hangs up (the child died) must not make the master spin.
#[test]
fn a_hangup_while_the_client_is_stalled_does_not_spin_the_master() {
    let t = TempDir::new();
    let (mut a, _) = start_env(
        t.path(),
        "hs",
        &["/bin/sh", "-c", "echo PID:$$; exec yes"],
        "me",
        &[("ACS_RING", "65536")],
    );
    a.wait_output("PID:", T);
    let out = String::from_utf8_lossy(&a.output).into_owned();
    let pid: i32 = out
        .split("PID:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {out:?}"));
    // Stall: the client socket, its buffer and the ring fill; `yes` blocks.
    a.set_paused(true);
    std::thread::sleep(Duration::from_millis(1500));
    // The child dies; its pty hangs up while nothing can be read.
    acs::sys::kill(pid, libc::SIGKILL).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let master = acs::testutil::session_pid(&sock(t.path(), "hs")).unwrap();
    let burnt = acs::testutil::cpu_over(master, Duration::from_secs(1));
    assert!(
        burnt < Duration::from_millis(300),
        "master used {burnt:?} of CPU in 1 s while waiting"
    );
    // Reading again, the client gets the rest and the exit.
    a.set_paused(false);
    let status = wait_exit(&mut a);
    assert_eq!(status & 0x7f, libc::SIGKILL, "{status:#x}");
}

/// Regression (acs-d1v): frames a taken-over client still sends — INPUT,
/// RESIZE, KILL — are ignored, and the new client's input is not mistaken
/// for a duplicate.
#[test]
fn a_taken_over_client_is_not_listened_to() {
    let t = TempDir::new();
    // Much output at once, so a stalled old client keeps its connection
    // open (Closing, with output queued) after the takeover.
    let (mut a, wa) = start(
        t.path(),
        "to",
        &[
            "/bin/sh",
            "-c",
            "stty -echo; sleep 0.3; head -c 400000 /dev/zero | tr '\\0' y; echo; while read l; do echo got:$l; done",
        ],
        "me@one",
    );
    a.set_paused(true);
    std::thread::sleep(Duration::from_millis(1000));
    let (mut b, m) = attach(t.path(), "to", "me@one", None);
    let Msg::Welcome(wb) = m else { panic!("{m:?}") };
    // The old client, not yet gone, still sends — then leaves.
    let _ = a.try_send(&Msg::Input {
        seq: wa.input_seq,
        bytes: b"evil\r".to_vec(),
    });
    let _ = a.try_send(&Msg::Resize(acs::proto::WinSize {
        cols: 20,
        rows: 5,
        xpixel: 0,
        ypixel: 0,
    }));
    let _ = a.try_send(&Msg::Kill {
        identity: "me".into(),
        force: true,
    });
    drop(a);
    std::thread::sleep(Duration::from_millis(300));
    // The session lives, and the new client's first input counts.
    b.send(&Msg::Input {
        seq: wb.input_seq,
        bytes: b"good\r".to_vec(),
    });
    b.wait_output("got:good", T);
    let out = String::from_utf8_lossy(&b.output).into_owned();
    assert!(!out.contains("got:evil"), "{out:?}");
    assert!(acs::testutil::session_pid(&sock(t.path(), "to")).is_ok());
}

/// acs-fbo: a KILL that has not said who it is gets nowhere, and a
/// connection the master has just refused with BUSY cannot destroy the very
/// session it was denied. Before this, five bytes on the socket did it.
#[test]
fn a_kill_from_outside_is_refused_while_someone_else_is_attached() {
    let t = TempDir::new();
    let (mut alice, _) = start(
        t.path(),
        "s",
        &["/bin/sh", "-c", "echo armed; sleep 1000"],
        "alice@laptop",
    );
    alice.wait_output("armed", T);

    // Someone else asks to attach and is told the session is taken.
    let mut bob = FrameConn::connect(&sock(t.path(), "s")).unwrap();
    bob.send(&Msg::Hello(hello("s", Mode::Attach, "bob@desk")));
    assert!(
        matches!(bob.recv_control(T), Some(Msg::Busy { identity, .. }) if identity == "alice@laptop")
    );

    // On that same refused connection, a kill is refused the same way.
    bob.send(&Msg::Kill {
        identity: "bob@desk".into(),
        force: false,
    });
    assert!(
        matches!(bob.recv_control(T), Some(Msg::Busy { identity, .. }) if identity == "alice@laptop"),
        "a refused client ended the session it was denied"
    );

    // An anonymous kill is refused too: it names nobody, so it is nobody.
    let mut anon = FrameConn::connect(&sock(t.path(), "s")).unwrap();
    anon.send(&Msg::Kill {
        identity: String::new(),
        force: false,
    });
    assert!(matches!(anon.recv_control(T), Some(Msg::Busy { .. })));

    // Alice's session is still there, and still hers.
    std::thread::sleep(Duration::from_millis(300));
    assert!(sock(t.path(), "s").exists(), "the session was ended");
    alice.send(&Msg::Input {
        seq: 0,
        bytes: b"echo alive\r".to_vec(),
    });
    alice.wait_output("alive", T);
}

/// acs-fbo: agreeing to take the session ends it, as `--force` does for a
/// takeover, and the asker's connection is held until the master is gone.
#[test]
fn a_kill_from_outside_goes_through_with_force_or_the_same_identity() {
    let t = TempDir::new();
    let (mut alice, _) = start(
        t.path(),
        "s",
        &["/bin/sh", "-c", "echo armed; sleep 1000"],
        "alice@laptop",
    );
    alice.wait_output("armed", T);

    let mut bob = FrameConn::connect(&sock(t.path(), "s")).unwrap();
    bob.send(&Msg::Kill {
        identity: "bob@desk".into(),
        force: true,
    });
    // Held open until the session is really gone, then closed.
    assert!(bob.closed(T), "the killer's connection was not held");
    wait_gone(&sock(t.path(), "s"));

    // And a second terminal of the same person needs no force.
    let (mut me, _) = start(
        t.path(),
        "s2",
        &["/bin/sh", "-c", "echo armed; sleep 1000"],
        "alice@laptop",
    );
    me.wait_output("armed", T);
    let mut other = FrameConn::connect(&sock(t.path(), "s2")).unwrap();
    other.send(&Msg::Kill {
        identity: "alice@laptop".into(),
        force: false,
    });
    assert!(other.closed(T));
    wait_gone(&sock(t.path(), "s2"));
}
