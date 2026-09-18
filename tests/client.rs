//! The client over a local transport (acs-5v9.15): attach, byte-exact
//! output, detach and re-attach, exit, status codes, terminal restore.

mod common;

use std::time::Duration;

use common::*;

fn sh(cmd: &str) -> Vec<String> {
    vec!["--".into(), "/bin/sh".into(), "-c".into(), cmd.into()]
}

fn start(remote: &Remote, session: &str, cmd: &str) -> Client {
    let mut args = vec!["devbox".to_string(), session.to_string()];
    args.extend(sh(cmd));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Client::start(remote, &args)
}

#[test]
fn output_is_byte_exact_including_escape_sequences() {
    let remote = Remote::installed();
    let data_file = remote.root.path().join("data");
    let data = binary_pattern(&data_file, 300_000);
    let mut c = start(
        &remote,
        "bx",
        &format!(
            "stty raw -echo; printf 'BEGIN'; cat '{}'; printf 'END'; sleep 30",
            data_file.display()
        ),
    );
    c.wait_for("END", T);
    let out = c.output();
    let begin = out.windows(5).position(|w| w == b"BEGIN").unwrap() + 5;
    assert_eq!(&out[begin..begin + data.len()], &data[..], "bytes changed");
    assert_eq!(&out[begin + data.len()..begin + data.len() + 3], b"END");
}

#[test]
fn new_session_is_announced_and_raw_mode_applies() {
    let remote = Remote::installed();
    let mut c = start(&remote, "ann", "echo ready; sleep 30");
    c.wait_for("new session 'ann' on devbox", T);
    c.wait_for("ready", T);
    assert!(!c.echo_on(), "client terminal should be raw");
}

#[test]
fn detach_leaves_the_session_and_reattach_finds_it() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "work",
        "echo started; while read l; do echo got:$l; done",
    );
    c.wait_for("started", T);
    c.send(b"one\r");
    c.wait_for("got:one", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    c.wait_for(
        "detached from devbox/work — reattach with: acs devbox work",
        T,
    );
    assert!(c.echo_on(), "terminal restored after detach");
    assert!(remote.session_exists("work"));

    let mut c2 = Client::start(&remote, &["devbox", "work"]);
    // A fresh attach clears the screen and asks the program to redraw.
    c2.wait_for("\x1b[H\x1b[J", T);
    c2.send(b"two\r");
    c2.wait_for("got:two", T);
    assert!(!c2.text().contains("new session"));
}

#[test]
fn exit_command_ends_the_session() {
    let remote = Remote::installed();
    let mut c = start(&remote, "gone", "echo started; sleep 1000");
    c.wait_for("started", T);
    c.send(&command(b'x'));
    // The shell dies of SIGHUP: 128 + 1.
    assert_eq!(c.wait(T), 129);
    assert!(c.echo_on());
    let deadline = std::time::Instant::now() + T;
    while remote.session_exists("gone") {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_programs_exit_status_is_the_clients() {
    let remote = Remote::installed();
    let mut c = start(&remote, "st", "echo bye; exit 7");
    c.wait_for("bye", T);
    assert_eq!(c.wait(T), 7);
    assert!(c.echo_on());
}

#[test]
fn single_escape_press_and_other_keys_reach_the_program() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "keys",
        "stty raw -echo; echo armed; od -An -tx1 -N 4",
    );
    c.wait_for("armed", T);
    // Ctrl-] then 'a' goes through at once; Ctrl-] alone after the window.
    c.send(&[0x1d, b'a']);
    std::thread::sleep(Duration::from_millis(50));
    c.send(&[0x1d]);
    std::thread::sleep(Duration::from_millis(600));
    c.send(b"z");
    c.wait_for("7a", T);
    let text = c.text();
    let words: Vec<&str> = text[text.find("armed").unwrap()..]
        .split_whitespace()
        .collect();
    assert!(
        words.windows(4).any(|w| w == ["1d", "61", "1d", "7a"]),
        "{words:?}"
    );
}

#[test]
fn resize_reaches_the_program() {
    let remote = Remote::installed();
    let mut c = start(&remote, "rsz", "echo ready; read x; stty size");
    c.wait_for("ready", T);
    c.resize(101, 33);
    std::thread::sleep(Duration::from_millis(200));
    c.send(b"\r");
    c.wait_for("33 101", T);
}

#[test]
fn modes_left_on_are_reset_on_detach() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "modes",
        "printf '\\033[?1049h\\033[?1000;1006h\\033[?2004hTUI'; sleep 30",
    );
    c.wait_for("TUI", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let out = c.text();
    let tail = &out[out.rfind("TUI").unwrap()..];
    for reset in ["\x1b[?1000l", "\x1b[?1006l", "\x1b[?2004l", "\x1b[?1049l"] {
        assert!(tail.contains(reset), "missing {reset:?} in {tail:?}");
    }
}

#[test]
fn new_flag_creates_numbered_sessions() {
    let remote = Remote::installed();
    let mut a = Client::start(
        &remote,
        &["devbox", "--new", "--", "/bin/sh", "-c", "sleep 30"],
    );
    a.wait_for("new session '1' on devbox", T);
    let mut b = Client::start(
        &remote,
        &["devbox", "--new", "--", "/bin/sh", "-c", "sleep 30"],
    );
    b.wait_for("new session '2' on devbox", T);
}

#[test]
fn unsupported_remote_platform_is_reported() {
    // A missing binary is installed (tests/install.rs); a platform acs has
    // no build for is an error.
    let remote = Remote::new();
    remote.fake_uname("SunOS", "i86pc");
    let mut c = Client::start(&remote, &["devbox"]);
    assert_eq!(c.wait(T), 254);
    c.wait_for("devbox runs SunOS i86pc, which acs does not support", T);
}

#[test]
fn failing_transport_is_unreachable() {
    let remote = Remote::installed();
    let mut c = Client::start_env(&remote, &["--transport-cmd", "false", "devbox"], &[]);
    assert_eq!(c.wait(T), 255);
    c.wait_for("closed before acs started", T);
}
