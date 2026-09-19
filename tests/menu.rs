//! A plain `acs <host>` (acs-s0g, DESIGN §4.4): a session is created when
//! none is detached; otherwise a menu picks one to attach, ends some, or
//! leaves. The keys themselves are unit-tested in `src/menu.rs`.

mod common;

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;

/// A session that answers each line typed with `got-<name>:<line>`.
fn echo(name: &str) -> String {
    format!("echo ready-{name}; while read l; do echo got-{name}:$l; done")
}

/// Start session `name` running [`echo`] and detach from it.
fn detached(remote: &Remote, name: &str) {
    let mut c = Client::start(
        remote,
        &["devbox", name, "--", "/bin/sh", "-c", &echo(name)],
    );
    c.wait_for(&format!("ready-{name}"), T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
}

/// Wait for the menu to be up.
fn menu_up(c: &mut Client) {
    c.wait_for("\x1b[?1049h", T);
    c.wait_for("acs: detached sessions on devbox", T);
}

/// After the menu is left for a session: it is attached (a fresh attach
/// clears the screen) and answers as `name`.
fn attached_to(c: &mut Client, name: &str) {
    c.wait_for("\x1b[?1049l", T);
    c.wait_for("\x1b[H\x1b[J", T);
    c.send(b"hi\r");
    c.wait_for(&format!("got-{name}:hi"), T);
}

/// The last screen the menu drew.
fn last_screen(c: &Client) -> String {
    let text = c.text();
    text[text.rfind("\x1b[Hacs: ").expect("no menu drawn")..].to_string()
}

#[test]
fn with_no_session_it_creates_main() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &["devbox", "--", "/bin/sh", "-c", "echo up; sleep 30"],
    );
    c.wait_for("new session 'main' on devbox", T);
    c.wait_for("up", T);
    assert!(!c.text().contains("\x1b[?1049h"), "no menu: {:?}", c.text());
}

#[test]
fn with_no_session_detached_it_creates_one_named_as_free() {
    let remote = Remote::installed();
    let mut a = Client::start(
        &remote,
        &["devbox", "main", "--", "/bin/sh", "-c", "echo a; sleep 30"],
    );
    a.wait_for("a", T);
    // main is attached: the lowest free number, as --new.
    let mut b = Client::start(
        &remote,
        &["devbox", "--", "/bin/sh", "-c", "echo b; sleep 30"],
    );
    b.wait_for("new session '1' on devbox", T);
    b.wait_for("b", T);
    // ACS_DEFAULT_SESSION names it while that name is free.
    let mut c = Client::start_env(
        &remote,
        &["devbox", "--", "/bin/sh", "-c", "echo c; sleep 30"],
        &[("ACS_DEFAULT_SESSION", "mine")],
    );
    c.wait_for("new session 'mine' on devbox", T);
    for c in [&b, &c] {
        assert!(!c.text().contains("\x1b[?1049h"), "no menu: {:?}", c.text());
    }
}

#[test]
fn a_number_attaches_its_session() {
    let remote = Remote::installed();
    detached(&remote, "one");
    detached(&remote, "two");
    let mut c = Client::start(&remote, &["devbox"]);
    menu_up(&mut c);
    c.wait_for("  2  two ", T);
    c.send(b"2");
    attached_to(&mut c, "two");
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    c.wait_for("detached from devbox/two", T);
}

#[test]
fn the_cursor_and_enter_attach_the_selection() {
    let remote = Remote::installed();
    detached(&remote, "one");
    detached(&remote, "two");
    let mut c = Client::start(&remote, &["devbox"]);
    menu_up(&mut c);
    c.send(b"\x1b[B");
    c.wait_for("\x1b[7m> 2  two ", T);
    c.send(b"\r");
    attached_to(&mut c, "two");
}

#[test]
fn dot_shows_attached_sessions_and_takes_one_over_when_confirmed() {
    let remote = Remote::installed();
    detached(&remote, "idle");
    let mut a = Client::start(
        &remote,
        &["devbox", "busy", "--", "/bin/sh", "-c", &echo("busy")],
    );
    a.wait_for("ready-busy", T);
    // Someone else: the takeover needs --force, which the menu's yes gives.
    let mut c = Client::start_env(&remote, &["devbox"], &[("ACS_IDENTITY", "other@elsewhere")]);
    menu_up(&mut c);
    assert!(!last_screen(&c).contains("busy"), "{:?}", last_screen(&c));
    c.send(b".");
    c.wait_for("acs: all sessions on devbox", T);
    c.wait_for("  1  busy  attached  tester@local", T);
    c.send(b"1");
    c.wait_for(
        "session 'busy' is attached from tester@local — take over? [y/N]",
        T,
    );
    c.send(b"y");
    attached_to(&mut c, "busy");
    a.wait_for("another client took over devbox/busy", T);
    assert_eq!(a.wait(T), 3);
}

#[test]
fn x_ends_a_session_and_the_menu_shows_the_rest() {
    let remote = Remote::installed();
    detached(&remote, "one");
    detached(&remote, "two");
    let mut c = Client::start(&remote, &["devbox"]);
    menu_up(&mut c);
    c.send(b"x");
    c.wait_for("end session 'one'? y (or x) ends it", T);
    c.send(b"y");
    c.wait_for("session 'one' ended", T);
    assert!(!remote.session_exists("one"));
    let screen = last_screen(&c);
    assert!(!screen.contains(" one "), "{screen:?}");
    assert!(screen.contains("\x1b[7m> 1  two "), "{screen:?}");
    c.send(b"1");
    attached_to(&mut c, "two");
}

#[test]
fn esc_exit_and_ctrl_c_leave_with_the_terminal_restored() {
    let remote = Remote::installed();
    detached(&remote, "one");
    // Esc; the exit row (one, new session, exit); Ctrl-C.
    for (keys, status) in [(&b"\x1b"[..], 0), (b"jj\r", 0), (b"\x03", 130)] {
        let mut c = Client::start(&remote, &["devbox"]);
        menu_up(&mut c);
        c.send(keys);
        assert_eq!(c.wait(T), status, "{keys:?}");
        let text = c.text();
        assert!(text.ends_with("\x1b[?25h\x1b[?1049l"), "{keys:?}: {text:?}");
        assert!(c.echo_on(), "{keys:?}: terminal left raw");
    }
    assert!(remote.session_exists("one"));
}

#[test]
fn an_alias_is_resolved_once_for_the_list_and_the_attach() {
    let remote = Remote::installed();
    detached(&remote, "one");
    let ssh = Ssh::new(&[("you@devbox.lan", &remote)]);
    let net = Net::new(&["devbox.lan", "devbox.example.com"]);
    let env =
        net.env("hosts:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n");
    let path = ssh.path().display().to_string();
    let mut c = Client::spawn(&exe(), &["--ssh", &path, "you@devbox"], &refs(&env));
    c.wait_for("\x1b[?1049h", T);
    c.wait_for("acs: detached sessions on you@devbox", T);
    c.wait_for("> 1  one ", T);
    c.send(b"1");
    attached_to(&mut c, "one");
    assert_eq!(net.pinged(), ["devbox.lan"]);
    let dests: Vec<String> = ssh.keys().into_iter().map(|(d, _)| d).collect();
    assert_eq!(
        dests,
        ["you@devbox.lan", "you@devbox.lan"],
        "list, then attach"
    );
}

#[test]
fn without_a_terminal_for_the_menu_it_attaches_main() {
    let remote = Remote::installed();
    detached(&remote, "main");
    detached(&remote, "other");
    // Keys from a terminal, output to a pipe.
    let (pty, tty) = acs::sys::openpty().unwrap();
    let mut child = acs_cmd()
        .args(["--transport-cmd", &remote.transport(), "devbox"])
        .env("ACS_IDENTITY", "tester@local")
        .env_remove("ACS_DEFAULT_SESSION")
        .stdin(Stdio::from(tty.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(tty);
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut stdout = child.stdout.take().unwrap();
    let o = out.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            o.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let wait_for = |needle: &str| {
        let deadline = Instant::now() + T;
        while !String::from_utf8_lossy(&out.lock().unwrap()).contains(needle) {
            assert!(Instant::now() < deadline, "no {needle:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for("\x1b[H\x1b[J");
    acs::sys::write_all(pty.as_raw_fd(), b"hi\r").unwrap();
    wait_for("got-main:hi");
    let text = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
    assert!(!text.contains("\x1b[?1049h"), "no menu: {text:?}");
    let _ = child.kill();
    let _ = child.wait();
}
