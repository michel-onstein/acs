//! Per-phase connect timings under `-v` (acs-pgn): one line per phase, in
//! order, for the first connection, the menu's and every redial. The
//! greeting is one of them (acs-ftn): `HELLO sent` where the whole of it
//! has gone, which is before `ACS-READY seen` when it went out with the
//! dial (acs-trw) and after it when it did not.

mod common;

use common::*;

const ALIAS: &str = "aliases:\n  devbox:\n    - host: devbox.lan\n";

const TICKER: &str = "i=0; while true; do i=$((i+1)); printf '#%d#\\n' $i; sleep 0.01; done";

/// The phases `connection` told, in the order they were told.
fn phases(text: &str, connection: &str) -> Vec<String> {
    let tag = format!("acs: timing: {connection}: ");
    text.split(tag.as_str())
        .skip(1)
        .map(|rest| {
            let end = rest.find(" +").expect("a timing line has its step");
            let tail = &rest[end..];
            assert!(
                tail.split(['\r', '\n'])
                    .next()
                    .unwrap()
                    .ends_with(" ms total)"),
                "{tail:?}"
            );
            rest[..end].to_string()
        })
        .collect()
}

#[test]
fn the_first_connection_and_a_redial_tell_each_phase_in_order() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let mut env = net.env(ALIAS);
    env.extend(
        [
            ("ACS_BACKOFF_MS", "100"),
            ("ACS_PING_MS", "200"),
            ("ACS_DEAD_MS", "800"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let mut c = Client::start_env(
        &remote,
        &["-v", "devbox", "t", "--", "/bin/sh", "-c", TICKER],
        &refs(&env),
    );
    c.wait_for("timing: first connection: first output byte", T);
    assert_eq!(
        phases(&c.text(), "first connection"),
        [
            "alias resolved",
            "ssh spawned",
            "HELLO sent",
            "ACS-READY seen",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
    remote.cut_link();
    c.wait_for("timing: redial: first output byte", T);
    assert_eq!(
        phases(&c.text(), "redial"),
        [
            "alias resolved",
            "ssh spawned",
            "HELLO sent",
            "ACS-READY seen",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
    // Told once per connection, not for every output byte after the first.
    assert_eq!(phases(&c.text(), "first connection").len(), 6);
}

/// The menu's connection owes its whole greeting to `serve`: the proxy
/// read the list's frames from it first (DESIGN 4.4), so nothing went
/// ahead with the dial. `HELLO sent` is told where the greeting actually
/// goes -- after the list, not between `ssh spawned` and `ACS-READY seen`
/// as it is on a connection that greeted with its dial (acs-ftn).
#[test]
fn the_menus_connection_tells_the_session_list() {
    let remote = Remote::installed();
    let mut d = Client::start(
        &remote,
        &[
            "devbox",
            "one",
            "--",
            "/bin/sh",
            "-c",
            "echo ready; sleep 30",
        ],
    );
    d.wait_for("ready", T);
    d.send(&command(b'd'));
    assert_eq!(d.wait(T), 0);

    let mut c = Client::start(&remote, &["-v", "devbox"]);
    c.wait_for("acs: detached sessions on devbox", T);
    c.send(b"1");
    c.wait_for("timing: first connection: first output byte", T);
    assert_eq!(
        phases(&c.text(), "first connection"),
        [
            "ssh spawned",
            "ACS-READY seen",
            "session list received",
            "HELLO sent",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
}

#[test]
fn without_v_no_timing_is_told() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &["devbox", "q", "--", "/bin/sh", "-c", "echo up; sleep 30"],
    );
    c.wait_for("up", T);
    assert!(!c.text().contains("timing:"), "{:?}", c.text());
}

/// A greeting the transport's stdin pipe cannot take in one write: the
/// write in `dial` is non-blocking, so what does not fit stays in
/// `Link::pending` for `serve` (acs-trw). That case is not silent
/// (acs-ftn) -- `HELLO partly sent` is told at the dial, and `HELLO sent`
/// only where the last of the greeting goes, which is past the marker.
/// The HELLO carries the command to run, so a long enough one fills the
/// pipe; the padding lands on `sh -c` as positional parameters it ignores.
#[test]
fn a_greeting_too_big_for_the_pipe_is_told_twice() {
    let remote = Remote::installed();
    // Over any pipe buffer acs can be handed: 64 KiB on Linux, at most
    // that on macOS.
    let pad = "p".repeat(1024);
    let mut argv: Vec<&str> = vec![
        "-v",
        "devbox",
        "big",
        "--",
        "/bin/sh",
        "-c",
        "echo up; sleep 30",
    ];
    for _ in 0..192 {
        argv.push(&pad);
    }
    let mut c = Client::start(&remote, &argv);
    c.wait_for("timing: first connection: first output byte", T);
    assert_eq!(
        phases(&c.text(), "first connection"),
        [
            "ssh spawned",
            "HELLO partly sent",
            "ACS-READY seen",
            "HELLO sent",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
}
