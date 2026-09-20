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

/// acs-y5r: agreeing to take a session is about whoever was attached at
/// the time. It is spent by that attach, so a redial later — which may
/// find somebody else entirely — asks again rather than throwing them out
/// without a word.
#[test]
fn a_granted_takeover_is_not_reused_on_a_later_redial() {
    let remote = Remote::installed();
    let mut alice = client(&remote, "alice@laptop", &[]);
    alice.wait_for("up", T);

    // Bob takes it, agreeing once. A long backoff keeps him in the wait
    // after his link is cut, so Carol is reliably there when he redials.
    let mut bob = Client::start_env(
        &remote,
        &[&["devbox", "main", "--force"][..], CMD].concat(),
        &[("ACS_IDENTITY", "bob@desk"), ("ACS_BACKOFF_MS", "3000")],
    );
    assert_eq!(alice.wait(T), 3);
    bob.send(b"mine\r");
    bob.wait_for(&got("mine"), T);

    // The link drops, and Carol attaches to the session Bob left behind.
    remote.cut_link();
    let mut carol = client(&remote, "carol@pi", &[]);
    // A fresh attach clears the screen; typing before that is dropped.
    carol.wait_for("\x1b[H\x1b[J", T);
    carol.send(b"hers\r");
    carol.wait_for("got:", T);

    // Bob comes back and is asked, rather than silently taking it from
    // Carol on the strength of an agreement about Alice.
    bob.wait_for("take over? [y/N] ", T);
    drop(carol);
}
