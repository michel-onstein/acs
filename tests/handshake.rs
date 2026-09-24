//! The handshake over a link (DESIGN §5.3, acs-trw): the HELLO goes into
//! the transport's stdin the moment it is spawned, so the marker and the
//! WELCOME come back together instead of one after the other.

mod common;

use common::*;

/// A remote that says nothing until the client has spoken: with the HELLO
/// sent only after `ACS-READY`, neither side would ever start. The login
/// noise then arrives ahead of the marker, as a chatty `.bashrc` does, and
/// is still discarded.
#[test]
fn the_hello_goes_out_with_the_dial_not_after_the_marker() {
    let remote = Remote::installed();
    remote.mute_until_greeted();
    remote.login_noise("Welcome to devbox!\nYou have mail.\n");
    let mut c = Client::start(
        &remote,
        &["devbox", "hs", "--", "/bin/sh", "-c", "echo up; sleep 30"],
    );
    c.wait_for("up", T);
    assert!(!c.text().contains("Welcome to devbox"), "{:?}", c.text());
}

/// The redial greets the same way: the resume is the common case, and it
/// is the one that pays the round trip on every drop.
#[test]
fn a_redial_greets_before_the_marker_too() {
    let remote = Remote::installed();
    remote.mute_until_greeted();
    remote.login_noise("motd\n");
    let mut c = Client::start_env(
        &remote,
        &[
            "devbox",
            "hsr",
            "--",
            "/bin/sh",
            "-c",
            "echo up; read x; echo back",
        ],
        &[("ACS_BACKOFF_MS", "100")],
    );
    c.wait_for("up", T);
    remote.cut_link();
    c.wait_resumed();
    c.send(b"\r");
    c.wait_for("back", T);
    assert!(!c.text().contains("motd"), "{:?}", c.text());
}

/// The HELLO carries the terminal's size, and it now leaves before ssh has
/// even connected: a window resized while the dial is in flight raises a
/// SIGWINCH that no RESIZE follows, since only a welcomed link sends them.
/// The client catches it up on the WELCOME, so the program starts on the
/// size the terminal has.
#[test]
fn a_window_resized_while_the_dial_is_held_reaches_the_program() {
    let remote = Remote::installed();
    remote.hold_dial();
    let dialled = remote.connections();
    let mut c = Client::start(
        &remote,
        &[
            "devbox",
            "hsz",
            "--",
            "/bin/sh",
            "-c",
            "while :; do stty size; sleep 0.2; done",
        ],
    );
    // The HELLO was built before the transport was spawned, so by the time
    // there is a connection to hold it is already written: the resize is
    // the one the handshake cannot carry.
    remote.wait_more_connections(dialled, 1, T);
    c.resize(101, 33);
    remote.release_dial();
    c.wait_for("33 101", T);
}
