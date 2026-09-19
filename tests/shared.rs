//! One account, several people (acs-5v9.18, DESIGN §4.5): the same identity
//! takes over silently, another identity is asked first.

mod common;

use common::*;

const CMD: &[&str] = &[
    "--",
    "/bin/sh",
    "-c",
    "echo up; while read l; do echo got:$l; done",
];

/// What the program prints for a line typed after taking over: a takeover
/// is a re-attach, so the client's Ctrl-L comes first (DESIGN §5.2).
fn got(line: &str) -> String {
    format!("got:\x0c{line}")
}

fn client(remote: &Remote, identity: &str, extra: &[&str]) -> Client {
    let mut args = vec!["devbox", "main"];
    args.extend_from_slice(extra);
    args.extend_from_slice(CMD);
    Client::start_env(remote, &args, &[("ACS_IDENTITY", identity)])
}

#[test]
fn same_identity_takes_over_silently() {
    let remote = Remote::installed();
    let mut a = client(&remote, "me@one", &[]);
    a.wait_for("up", T);
    let mut b = client(&remote, "me@one", &[]);
    assert_eq!(a.wait(T), 3);
    a.wait_for(
        "another client took over devbox/main — reattach with: acs devbox main",
        T,
    );
    assert!(a.echo_on());
    b.send(b"x\r");
    b.wait_for(&got("x"), T);
    assert!(!b.text().contains("take over?"));
}

#[test]
fn other_identity_is_asked_and_can_decline() {
    let remote = Remote::installed();
    let mut a = client(&remote, "alice@laptop", &[]);
    a.wait_for("up", T);
    let mut b = client(&remote, "bob@desk", &[]);
    b.wait_for(
        "session 'main' on devbox is attached from alice@laptop since ",
        T,
    );
    b.wait_for("take over? [y/N] ", T);
    b.send(b"n\r");
    assert_eq!(b.wait(T), 3);
    // Alice is untouched.
    a.send(b"still\r");
    a.wait_for("got:still", T);
}

#[test]
fn other_identity_can_accept() {
    let remote = Remote::installed();
    let mut a = client(&remote, "alice@laptop", &[]);
    a.wait_for("up", T);
    let mut b = client(&remote, "bob@desk", &[]);
    b.wait_for("take over? [y/N] ", T);
    b.send(b"y\r");
    assert_eq!(a.wait(T), 3);
    b.send(b"mine\r");
    b.wait_for(&got("mine"), T);
}

#[test]
fn force_skips_the_question() {
    let remote = Remote::installed();
    let mut a = client(&remote, "alice@laptop", &[]);
    a.wait_for("up", T);
    let mut b = client(&remote, "bob@desk", &["--force"]);
    assert_eq!(a.wait(T), 3);
    b.send(b"f\r");
    b.wait_for(&got("f"), T);
    assert!(!b.text().contains("take over?"));
}

#[test]
fn default_session_comes_from_the_environment() {
    let remote = Remote::installed();
    let mut c = Client::start_env(
        &remote,
        &["devbox", "--", "/bin/sh", "-c", "echo up; sleep 30"],
        &[("ACS_DEFAULT_SESSION", "michel")],
    );
    c.wait_for("new session 'michel' on devbox", T);
}
