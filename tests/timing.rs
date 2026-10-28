//! Per-phase connect timings under `-v` (acs-pgn): one line per phase, in
//! order, for the first connection, the menu's and every redial. The
//! greeting is one of them (acs-ftn): `HELLO sent` where the whole of it
//! has gone, which is before `ACS-READY seen` when it went out with the
//! dial (acs-trw) and after it when it did not.

mod common;

use acs::timing::{told, Phase};
use common::*;

const ALIAS: &str = "aliases:\n  devbox:\n    - host: devbox.lan\n";

/// The phases `connection` told, in the order they were told.
fn phases<'a>(text: &'a str, connection: &str) -> Vec<&'a str> {
    let all = told(text);
    let lines = text.matches("acs: timing: ").count();
    assert_eq!(
        all.len(),
        lines,
        "a timing line that does not parse: {text:?}"
    );
    all.into_iter()
        .filter(|t| t.what == connection)
        .map(|t| t.phase)
        .collect()
}

/// DESIGN §7's list of phases, in its order: the `` - `<phase>` `` items
/// under the bullet that names `timing::Phase`.
fn design_phases(design: &str) -> Vec<String> {
    let mut lines = design
        .lines()
        .skip_while(|l| !l.contains("`timing::Phase`"));
    assert!(lines.next().is_some(), "DESIGN §7 names `timing::Phase`");
    lines
        .skip_while(|l| !l.starts_with("  - `"))
        .take_while(|l| !l.is_empty() && l.starts_with("  "))
        .filter_map(|l| l.strip_prefix("  - `"))
        .map(|l| l.split('`').next().unwrap().to_string())
        .collect()
}

/// acs-fb7r: the phases `-v` can tell are the ones DESIGN lists, in the
/// same order — a phase added to `Phase` without the doc fails here.
#[test]
fn designs_list_of_phases_is_timing_phase() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let design = std::fs::read_to_string(root.join("docs/DESIGN.md")).unwrap();
    let names: Vec<_> = Phase::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(design_phases(&design), names);
}

#[test]
fn the_first_connection_and_a_redial_tell_each_phase_in_order() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let mut env = net.env(ALIAS);
    env.extend(FAST.iter().map(|(k, v)| (k.to_string(), v.to_string())));
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

/// With a session detached, the HELLO that went ahead with the menu's dial
/// (acs-xvg) is moot: the connection owes the greeting of the session the
/// user picks to `serve`, and `HELLO sent` is told again where that goes
/// -- after the list (acs-ftn).
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
            "HELLO sent",
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

/// acs-xvg: with nothing detached, plain `acs <host>` costs no round trip
/// for the attach -- the HELLO went ahead with the dial, the proxy attached
/// it on finding nothing detached, and the WELCOME follows the list with
/// no second HELLO in between.
#[test]
fn with_nothing_detached_the_welcome_follows_the_list_without_a_second_hello() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &["-v", "devbox", "--", "/bin/sh", "-c", "echo up; sleep 30"],
    );
    c.wait_for("timing: first connection: first output byte", T);
    assert_eq!(
        phases(&c.text(), "first connection"),
        [
            "ssh spawned",
            "HELLO sent",
            "ACS-READY seen",
            "session list received",
            "WELCOME received",
            "first output byte",
        ],
        "{:?}",
        c.text()
    );
    let text = c.text();
    assert!(text.contains("new session 'main' on devbox"), "{text:?}");
    assert!(text.contains("_proxy --pick --eager"), "{text:?}");
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
