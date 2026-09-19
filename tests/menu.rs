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
/// clears the screen) and answers as `name`. Every session the menu offers
/// already existed, so the attach sends Ctrl-L first (redraw_on_reconnect)
/// and the line the program reads starts with it.
fn attached_to(c: &mut Client, name: &str) {
    c.wait_for("\x1b[?1049l", T);
    c.wait_for("\x1b[H\x1b[J", T);
    c.send(b"hi\r");
    c.wait_for(&format!("got-{name}:\x0chi"), T);
}

/// The last screen the menu drew.
fn last_screen(c: &Client) -> String {
    let text = c.text();
    // The last one drawn in full: a redraw may be read half-way, so skip
    // one that has not reached its closing erase yet.
    let mut screens: Vec<&str> = text.split("\x1b[Hacs: ").skip(1).collect();
    if screens.last().is_some_and(|s| !s.contains("\x1b[J")) && screens.len() > 1 {
        screens.pop();
    }
    let last = screens.last().expect("no menu drawn");
    format!("\x1b[Hacs: {last}")
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
        net.env("aliases:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n");
    let path = ssh.path().display().to_string();
    let mut c = Client::spawn(&exe(), &["--ssh", &path, "you@devbox"], &refs(&env));
    c.wait_for("\x1b[?1049h", T);
    c.wait_for("acs: detached sessions on you@devbox", T);
    c.wait_for("> 1  one ", T);
    c.send(b"1");
    attached_to(&mut c, "one");
    assert_eq!(net.pinged(), ["devbox.example.com", "devbox.lan"]);
    let dests: Vec<String> = ssh.keys().into_iter().map(|(d, _)| d).collect();
    assert_eq!(dests, ["you@devbox.lan"], "list and attach on one call");
}

/// acs-68z: the menu's connection sits idle while the user reads it; if it
/// drops meanwhile, the pick still attaches, over a connection of its own.
#[test]
fn a_menu_connection_lost_while_reading_is_dialed_afresh() {
    let remote = Remote::installed();
    detached(&remote, "one");
    let mut c = Client::start(&remote, &["devbox"]);
    menu_up(&mut c);
    let before = remote.transport_pids().len();
    remote.cut_link();
    std::thread::sleep(Duration::from_millis(200));
    c.send(b"1");
    attached_to(&mut c, "one");
    assert_eq!(remote.transport_pids().len(), before + 1);
    assert!(!c.text().contains("connection lost"), "{:?}", c.text());
}

/// acs-68z: the list, ending a session with `x`, showing the attached ones
/// with `.`, and the attach all go over the one ssh connection the menu
/// opened (`_proxy --pick`).
#[test]
fn the_menu_ends_and_attaches_over_one_ssh_connection() {
    let remote = Remote::installed();
    detached(&remote, "one");
    detached(&remote, "two");
    let ssh = Ssh::new(&[("devbox", &remote)]);
    let path = ssh.path().display().to_string();
    let mut c = Client::spawn(&exe(), &["-v", "--ssh", &path, "devbox"], &[]);
    menu_up(&mut c);
    c.send(b"x");
    c.wait_for("end session 'one'? y (or x) ends it", T);
    c.send(b"y");
    c.wait_for("session 'one' ended", T);
    assert!(!remote.session_exists("one"));
    c.send(b".");
    c.wait_for("acs: all sessions on devbox", T);
    c.send(b"1");
    attached_to(&mut c, "two");
    let calls = ssh.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains("_proxy --pick"), "{calls:?}");
    let running = c.text().matches("acs: running ").count();
    assert_eq!(running, 1, "{:?}", c.text());
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
    // `main` existed, so the attach sent Ctrl-L before the line.
    wait_for("got-main:\x0chi");
    let text = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
    assert!(!text.contains("\x1b[?1049h"), "no menu: {text:?}");
    let _ = child.kill();
    let _ = child.wait();
}

// ---- acs list in a terminal: every host (acs-uxj) ---------------------------

const TWO_HOSTS: &str = "\
aliases:
  devbox:
    - host: devbox.lan
  nas:
    - host: nas.lan
  old:
    - host: old.lan
";

/// devbox and nas, reached through the fake `ssh`, with a session detached
/// on each; old answers no ping. The client runs `acs list` in a terminal.
struct Every {
    devbox: Remote,
    nas: Remote,
    ssh: Ssh,
    net: Net,
}

impl Every {
    fn new() -> Every {
        let devbox = Remote::installed();
        let nas = Remote::installed();
        detached(&devbox, "main");
        detached(&nas, "work");
        let ssh = Ssh::new(&[("devbox.lan", &devbox), ("nas.lan", &nas)]);
        let net = Net::new(&["devbox.lan", "nas.lan"]);
        Every {
            devbox,
            nas,
            ssh,
            net,
        }
    }

    fn list(&self) -> Client {
        let env = self.net.env(TWO_HOSTS);
        let path = self.ssh.path().display().to_string();
        let mut c = Client::spawn(&exe(), &["list", "--ssh", &path], &refs(&env));
        c.wait_for("\x1b[?1049h", T);
        c.wait_for("acs: detached sessions on every host", T);
        c.wait_for("nas     work", T);
        c.wait_for("devbox  main", T);
        c
    }
}

#[test]
fn acs_list_in_a_terminal_picks_a_session_on_any_host() {
    let e = Every::new();
    let mut c = e.list();
    // Configuration order; the host no ping reached gets a line of its own.
    c.wait_for("old: no host for 'old' is reachable (tried old.lan)", T);
    let screen = last_screen(&c);
    assert!(
        screen.find("devbox  main") < screen.find("nas     work"),
        "{screen:?}"
    );
    assert!(!screen.contains("new session"), "{screen:?}");
    // nas's work is the second row: attach it there.
    c.send(b"2");
    attached_to(&mut c, "work");
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    c.wait_for("detached from nas/work", T);
    // Listed in BatchMode, then attached with the ordinary session call.
    let calls = e.ssh.calls();
    assert!(
        calls.iter().filter(|c| c.contains("BatchMode=yes")).count() >= 2,
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("nas.lan") && c.contains("--session work")),
        "{calls:?}"
    );
    assert!(e.devbox.session_exists("main"));
}

#[test]
fn acs_list_in_a_terminal_ends_and_creates_on_the_rows_host() {
    let e = Every::new();
    let mut c = e.list();
    // x on devbox's main ends it there, over a call of its own; nas keeps
    // its row. The cursor follows whichever host answered first, so it is
    // taken to the top (devbox's row) first.
    c.send(b"kk");
    c.wait_for("\x1b[7m> 1  devbox", T);
    c.send(b"x");
    c.wait_for("end session 'main' on devbox?", T);
    c.send(b"y");
    c.wait_for("session 'main' ended", T);
    assert!(!e.devbox.session_exists("main"));
    assert!(e.nas.session_exists("work"));
    let screen = last_screen(&c);
    assert!(screen.contains("no sessions on devbox"), "{screen:?}");
    assert!(screen.contains("\x1b[7m> 1  nas"), "{screen:?}");
    // n on nas's row: a new numbered session there.
    c.send(b"n");
    c.wait_for("\x1b[?1049l", T);
    c.wait_for("new session '1' on nas", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    assert!(e.nas.session_exists("1"));
}

#[test]
fn acs_list_of_one_host_in_a_terminal_is_its_menu_even_with_nothing_detached() {
    let remote = Remote::installed();
    let t = remote.transport();
    let mut c = Client::spawn(&exe(), &["list", "--transport-cmd", &t, "devbox"], &[]);
    c.wait_for("\x1b[?1049h", T);
    c.wait_for("acs: detached sessions on devbox", T);
    c.wait_for("n  new session", T);
    c.send(b"\x1b");
    assert_eq!(c.wait(T), 0);
    // Into a pipe it is the table, as ever.
    let out = acs_cmd()
        .args(["list", "--transport-cmd", &t, "devbox"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "no sessions on devbox\n"
    );
}
