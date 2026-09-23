//! Per-phase connect timings under `-v` (acs-pgn): one line per phase, in
//! order, for the first connection, the menu's and every redial.

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
            "ACS-READY seen",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
    // Told once per connection, not for every output byte after the first.
    assert_eq!(phases(&c.text(), "first connection").len(), 5);
}

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
