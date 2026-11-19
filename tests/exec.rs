//! Running a command as `ssh host command` does (DESIGN §7.7): on a
//! terminal in a session of its own that ends with it, or without one over
//! plain ssh, with stdin, stdout and stderr kept apart and byte-exact.

mod common;

use std::io::{Read, Write};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

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

/// `-T` drops the terminal even on one: the command runs on pipes, in a
/// session of its own that ends with it (DESIGN §5.5).
#[test]
fn dash_big_t_runs_without_a_terminal_in_a_session_that_ends_with_it() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &[
            "-T",
            "devbox",
            "--",
            "[ -t 0 ] || [ -t 1 ] || echo no-terminal",
        ],
    );
    assert_eq!(c.wait(T), 0, "{}", c.text());
    assert!(c.text().contains("no-terminal"), "{}", c.text());
    assert!(!c.text().contains("\x1b[?1049h"), "no menu: {}", c.text());
    wait_until(
        T,
        || !remote.session_exists("1"),
        || "the session ends with it",
    );
}

/// A piped acs whose stdin is written as the test goes, and whose stdout
/// and stderr are collected as they arrive, so a test can cut the link
/// in the middle of the stream.
struct Piped {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
}

impl Piped {
    fn start(remote: &Remote, args: &[&str]) -> Piped {
        let mut child = acs_cmd()
            .arg("--transport-cmd")
            .arg(remote.transport())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let collect = |mut r: Box<dyn Read + Send>| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let b = buf.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 65536];
                while let Ok(n) = r.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    b.lock().unwrap().extend_from_slice(&chunk[..n]);
                }
            });
            buf
        };
        let out = collect(Box::new(child.stdout.take().unwrap()));
        let err = collect(Box::new(child.stderr.take().unwrap()));
        Piped {
            stdin: child.stdin.take(),
            child,
            out,
            err,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn stdout_len(&self) -> usize {
        self.out.lock().unwrap().len()
    }

    fn stdout(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.err.lock().unwrap()).into_owned()
    }

    fn wait_stdout(&self, what: &[u8]) {
        wait_until(
            T,
            || self.stdout().windows(what.len()).any(|w| w == what),
            || {
                format!(
                    "waiting for {:?}; stderr: {}",
                    String::from_utf8_lossy(what),
                    self.stderr()
                )
            },
        );
    }

    /// The exit code, once stdout and stderr have been read to their end.
    fn wait(&mut self) -> i32 {
        self.close_stdin();
        let deadline = std::time::Instant::now() + T;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                // The readers finish at end of file, just after the exit.
                std::thread::sleep(std::time::Duration::from_millis(100));
                return st.code().unwrap_or(-1);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "acs did not exit; stderr: {}",
                self.stderr()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Piped {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Cut the link and wait for the redial, so the next cut (or the end) is
/// on the new link.
fn cut_and_wait_redial(remote: &Remote) {
    let base = remote.connections();
    remote.cut_link();
    remote.wait_more_connections(base, 1, T);
}

/// acs-182f: a piped command whose link drops in the middle of its output
/// resumes, with stdout byte for byte what it wrote — more than the ring
/// holds while nobody was attached — stderr apart, and its exit status.
#[test]
fn a_piped_command_resumes_after_a_dropped_link() {
    let remote = Remote::installed();
    let go = remote.home().join("go");
    let cmd = format!(
        "echo first; echo first-err >&2; while [ ! -f '{}' ]; do sleep 0.05; done; \
         seq 1 300000; echo last-err >&2; exit 3",
        go.display()
    );
    let mut p = Piped::start(&remote, &["-T", "devbox", "--", &cmd]);
    p.wait_stdout(b"first\n");
    cut_and_wait_redial(&remote);
    // The command goes on while the link is down: its output waits in the
    // master, which on pipes never lets the ring wrap.
    std::fs::write(&go, "").unwrap();
    assert_eq!(p.wait(), 3, "{}", p.stderr());
    let mut want = b"first\n".to_vec();
    for i in 1..=300000 {
        want.extend_from_slice(format!("{i}\n").as_bytes());
    }
    assert!(
        p.stdout() == want,
        "stdout differs ({} bytes, {} wanted)",
        p.stdout_len(),
        want.len()
    );
    assert_eq!(p.stderr(), "first-err\nlast-err\n");
    wait_until(
        T,
        || !remote.session_exists("1"),
        || "the session ends once its client has the end",
    );
}

/// acs-182f: the end of stdin reaches the command once and in its place —
/// after every byte before it, though the link dropped both before and
/// after it was sent.
#[test]
fn the_end_of_stdin_is_delivered_once_across_drops() {
    let remote = Remote::installed();
    remote.log_master();
    let got = remote.home().join("got");
    let go = remote.home().join("go");
    let cmd = format!(
        "cat > '{g}'; echo got $(wc -c < '{g}'); while [ ! -f '{go}' ]; do sleep 0.05; done; \
         cat; echo bye",
        g = got.display(),
        go = go.display()
    );
    let dir = acs::testutil::TempDir::new();
    let data = binary_pattern(&dir.path().join("data"), 200_000);
    let mut p = Piped::start(&remote, &["devbox", "--", &cmd]);
    p.write(&data[..100_000]);
    wait_until(
        T,
        || std::fs::metadata(&got).is_ok_and(|m| m.len() == 100_000),
        || "the first half arrives".to_string(),
    );
    cut_and_wait_redial(&remote);
    p.write(&data[100_000..]);
    p.close_stdin();
    p.wait_stdout(b"got 200000\n");
    // A drop after the end of input: the resume says it again, and the
    // second `cat` must see an end of file, not a hang or more bytes.
    cut_and_wait_redial(&remote);
    std::fs::write(&go, "").unwrap();
    assert_eq!(p.wait(), 0, "{}\n{}", p.stderr(), remote.master_log());
    assert_eq!(String::from_utf8_lossy(&p.stdout()), "got 200000\nbye\n");
    assert!(std::fs::read(&got).unwrap() == data, "stdin changed");
}

/// acs-182f: a large binary stream through the command, both ways at once
/// — `tar c | acs host -- tar x` — survives a drop in the middle.
#[test]
fn a_large_binary_transfer_survives_a_dropped_link() {
    let remote = Remote::installed();
    let dir = acs::testutil::TempDir::new();
    let data = binary_pattern(&dir.path().join("data"), 4_000_000);
    let mut p = Piped::start(&remote, &["devbox", "--", "cat"]);
    // The second half waits for the cut, so the link drops with the
    // stream going both ways rather than after it has all arrived.
    let (half, rest) = (data[..2_000_000].to_vec(), data[2_000_000..].to_vec());
    let (cut, go) = std::sync::mpsc::channel::<()>();
    let mut stdin = p.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        stdin.write_all(&half).unwrap();
        go.recv().unwrap();
        stdin.write_all(&rest).unwrap();
    });
    wait_until(
        T,
        || p.stdout_len() >= 1_000_000,
        || format!("a quarter through; {} bytes", p.stdout_len()),
    );
    cut_and_wait_redial(&remote);
    cut.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(p.wait(), 0, "{}", p.stderr());
    assert!(
        p.stdout() == data,
        "bytes changed ({} of {})",
        p.stdout_len(),
        data.len()
    );
}

/// A terminal never attaches a command on pipes: its stream is records,
/// not a screen (DESIGN §5.5).
#[test]
fn a_terminal_cannot_attach_a_piped_commands_session() {
    let remote = Remote::installed();
    let go = remote.home().join("go");
    let cmd = format!(
        "echo up; while [ ! -f '{}' ]; do sleep 0.05; done",
        go.display()
    );
    let mut p = Piped::start(&remote, &["-T", "devbox", "--", &cmd]);
    p.wait_stdout(b"up\n");
    let mut c = Client::start(&remote, &["devbox", "1"]);
    assert_ne!(c.wait(T), 0, "{}", c.text());
    assert!(c.text().contains("runs without a terminal"), "{}", c.text());
    std::fs::write(&go, "").unwrap();
    assert_eq!(p.wait(), 0, "{}", p.stderr());
}

/// Ctrl-C on a piped acs ends the command on the host, as killing ssh
/// does, rather than leaving it running for a client that is not coming
/// back; acs leaves with its status (SIGHUP, so 129).
#[test]
fn a_signal_to_a_piped_acs_ends_the_command() {
    let remote = Remote::installed();
    let mut p = Piped::start(&remote, &["devbox", "--", "echo up; sleep 60"]);
    p.wait_stdout(b"up\n");
    acs::sys::kill(p.child.id() as i32, libc::SIGINT).unwrap();
    assert_eq!(p.wait(), 129, "{}", p.stderr());
    wait_until(
        T,
        || !remote.session_exists("1"),
        || "the session ends with the command",
    );
}

/// A reader that stops reading (`| head`) ends the command, as a closed
/// ssh channel does.
#[test]
fn a_closed_stdout_ends_the_command() {
    let remote = Remote::installed();
    let mut child = acs_cmd()
        .args([
            "--transport-cmd",
            &remote.transport(),
            "-n",
            "devbox",
            "--",
            "yes",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut some = [0u8; 100];
    out.read_exact(&mut some).unwrap();
    assert_eq!(&some[..4], b"y\ny\n");
    drop(out);
    let _guard = acs::testutil::KillOnDrop(child);
    wait_until(
        T,
        || !remote.session_exists("1"),
        || "the session ends with the command",
    );
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
