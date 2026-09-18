//! `acs <host> --list` (acs-5v9.12).

mod common;

use common::*;

fn list(remote: &Remote) -> (i32, String) {
    let out = acs_cmd()
        .args(["--transport-cmd", &remote.transport(), "devbox", "--list"])
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn lists_attached_detached_and_drops_stale_sockets() {
    let remote = Remote::installed();
    let mut a = Client::start(
        &remote,
        &["devbox", "main", "--", "/bin/sh", "-c", "echo a; sleep 30"],
    );
    a.wait_for("a", T);
    let mut b = Client::start(
        &remote,
        &["devbox", "work", "--", "/bin/sh", "-c", "echo b; sleep 30"],
    );
    b.wait_for("b", T);
    b.send(&command(b'd'));
    assert_eq!(b.wait(T), 0);
    let stale = remote.sockets().join("old.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());

    let (code, out) = list(&remote);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("NAME"), "{out}");
    let row = |name: &str| {
        lines
            .iter()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no row for {name}:\n{out}"))
            .split_whitespace()
            .collect::<Vec<_>>()
    };
    assert_eq!(row("main")[1..3], ["attached", "tester@local"]);
    assert_eq!(row("work")[1..3], ["detached", "(tester@local)"]);
    assert!(row("work")
        .join(" ")
        .ends_with("/bin/sh -c echo b; sleep 30"));
    assert!(!out.contains("old"), "stale session listed:\n{out}");
    assert!(!stale.exists(), "stale socket not removed");
}

#[test]
fn empty_and_not_installed() {
    let remote = Remote::installed();
    assert_eq!(list(&remote), (0, "no sessions on devbox\n".into()));
    let bare = Remote::new();
    let (code, out) = list(&bare);
    assert_eq!(code, 0);
    assert!(out.contains("is not installed there"), "{out}");
}
