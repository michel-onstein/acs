//! Running a command as `ssh host command` does (DESIGN §7.7): on a
//! terminal in a session of its own that ends with it, or without one over
//! plain ssh, with stdin, stdout and stderr kept apart and byte-exact.

mod common;

use std::io::Write;
use std::process::{Output, Stdio};

use common::*;

/// `acs <args>` over `remote`'s stand-in transport, without a terminal:
/// stdin is `input`, stdout and stderr are captured.
fn piped(remote: &Remote, args: &[&str], input: &[u8]) -> Output {
    let mut cmd = acs_cmd();
    cmd.arg("--transport-cmd")
        .arg(remote.transport())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // Written from a thread, so a command that does not read cannot block
    // the test on a full pipe.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

#[test]
fn a_command_on_a_terminal_runs_in_its_own_session_and_exits_with_its_status() {
    let remote = Remote::installed();
    // The words are joined and read by the remote shell, as ssh does: the
    // `&&` is the shell's, not an argument of echo.
    let mut c = Client::start(
        &remote,
        &["devbox", "--", "echo", "one-shot", "&&", "exit", "7"],
    );
    assert_eq!(c.wait(T), 7, "{}", c.text());
    let text = c.text();
    assert!(text.contains("one-shot"), "{text}");
    assert!(!text.contains("&&"), "the shell read the line: {text}");
    // As quiet as ssh: no announcement, and no menu.
    assert!(!text.contains("new session"), "{text}");
    assert!(!text.contains("\x1b[?1049h"), "no menu: {text}");
    wait_until(
        T,
        || !remote.session_exists("1"),
        || "the session ends with it",
    );
}

/// The menu would attach a detached session and drop the command; a
/// command of its own never goes there.
#[test]
fn a_command_never_attaches_a_detached_session() {
    let remote = Remote::installed();
    let mut a = Client::start(&remote, &up_args("main"));
    a.wait_for("up", T);
    a.send(&command(b'd'));
    assert_eq!(a.wait(T), 0);
    assert!(remote.session_exists("main"));

    let mut c = Client::start(&remote, &["devbox", "--", "echo ran-it"]);
    assert_eq!(c.wait(T), 0, "{}", c.text());
    assert!(c.text().contains("ran-it"), "{}", c.text());
    assert!(!c.text().contains("\x1b[?1049h"), "no menu: {}", c.text());
    assert!(
        remote.session_exists("main"),
        "the detached session is left alone"
    );
}

#[test]
fn verbose_names_the_one_shot_session() {
    let remote = Remote::installed();
    let mut c = Client::start(&remote, &["-v", "devbox", "--", "true"]);
    assert_eq!(c.wait(T), 0, "{}", c.text());
    assert!(
        c.text().contains("new session '1' on devbox"),
        "{}",
        c.text()
    );
}

/// A session's command goes through the login shell too (DESIGN §4.2):
/// `$SHELL -c '<words>'`, so a pipe in it is a pipe.
#[test]
fn a_named_sessions_command_is_read_by_the_login_shell() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &["devbox", "piped", "--", "echo abc | tr a-c x-z; exit 3"],
    );
    assert_eq!(c.wait(T), 3, "{}", c.text());
    assert!(c.text().contains("xyz"), "{}", c.text());
}

#[test]
fn without_a_terminal_stdout_and_stderr_are_kept_apart_and_exact() {
    let remote = Remote::installed();
    let out = piped(
        &remote,
        &["devbox", "--", "tr a-z A-Z; echo oops >&2; exit 5"],
        b"hello\nworld\n",
    );
    assert_eq!(out.status.code(), Some(5));
    // No pty: no \r\n, and stdin's end reached the command (tr finished).
    assert_eq!(out.stdout, b"HELLO\nWORLD\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
}

#[test]
fn without_a_terminal_binary_data_passes_through_unchanged() {
    let remote = Remote::installed();
    let dir = acs::testutil::TempDir::new();
    let data = binary_pattern(&dir.path().join("data"), 200_000);
    let out = piped(&remote, &["devbox", "--", "cat"], &data);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout == data, "bytes changed");
}

#[test]
fn dash_n_reads_stdin_from_dev_null() {
    let remote = Remote::installed();
    let out = piped(
        &remote,
        &["-n", "devbox", "--", "cat; echo end"],
        b"not read\n",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"end\n");
}

/// `-T` drops the terminal even on one, and no session is made.
#[test]
fn dash_big_t_runs_without_a_terminal_and_without_a_session() {
    let remote = Remote::installed();
    // (The stand-in transport hands the remote this terminal as it is, so
    // whether ssh -T gives the command a pipe is ssh's to show, in
    // tests/e2e_ssh.rs; what acs decides is that no session is made.)
    let mut c = Client::start(&remote, &["-v", "-T", "devbox", "--", "echo ran-it"]);
    assert_eq!(c.wait(T), 0, "{}", c.text());
    let text = c.text();
    assert!(text.contains("ran-it"), "{text}");
    assert!(
        !text.contains("new session"),
        "-v names a session made: {text}"
    );
    assert!(
        !text.contains("_proxy"),
        "acs was not run on the remote: {text}"
    );
    assert!(!remote.session_exists("1"));
}

#[test]
fn a_forward_without_a_terminal_is_refused() {
    let remote = Remote::installed();
    let out = piped(
        &remote,
        &["-T", "-L", "8080:localhost:80", "devbox", "--", "true"],
        b"",
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("-L"));
}
