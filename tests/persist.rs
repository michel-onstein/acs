//! Persisting (acs-txt, DESIGN §5.3): with `--persist` or `persist: true`
//! a lost host is never given up on. It is pinged every
//! `reachability_interval` (the fake `ping` of `Net`, `ACS_PING`) and dialled
//! as soon as it answers — at the first connect and after a drop.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use acs::testutil::TempDir;
use common::*;

fn session(host: &str, extra: &[&'static str]) -> Vec<&'static str> {
    let mut args: Vec<&'static str> = extra.to_vec();
    args.push(Box::leak(host.to_string().into_boxed_str()));
    args.extend([
        "s",
        "--",
        "/bin/sh",
        "-c",
        "echo up; while read l; do echo got:$l; done",
    ]);
    args
}

/// Wait until `f` holds.
fn until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + T;
    while !f() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

const ALIAS: &str = "\
reachability_interval: 200ms
aliases:
  devbox:
    - host: devbox.lan
";

#[test]
fn an_alias_with_no_host_answering_is_waited_for_from_the_start() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(ALIAS);
    let mut c = Client::start_env(&remote, &session("devbox", &["--persist"]), &refs(&env));
    c.wait_for(
        "acs: waiting for devbox to answer a ping, every 200ms (Ctrl-C gives up)",
        T,
    );
    // Pinged again every interval, and nothing dialled meanwhile.
    until("three pings", || net.pinged().len() >= 3);
    assert!(
        remote.transport_pids().is_empty(),
        "dialled before an answer"
    );
    net.set_up(&["devbox.lan"]);
    c.wait_for("up", T);
    c.send(b"hi\r");
    c.wait_for("got:hi", T);
}

#[test]
fn without_persisting_the_same_start_gives_up() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(ALIAS);
    let mut c = Client::start_env(&remote, &session("devbox", &[]), &refs(&env));
    assert_eq!(c.wait(T), 255);
}

#[test]
fn a_dropped_plain_host_is_pinged_until_it_answers_then_resumed() {
    let remote = Remote::installed();
    // A plain host (no alias), persisting from the configuration.
    let net = Net::new(&["devbox"]);
    let env = net.env("persist: true\nreachability_interval: 200ms\n");
    let mut env = refs(&env);
    env.push(("ACS_PING_MS", "200"));
    let mut c = Client::start_env(&remote, &session("devbox", &[]), &env);
    c.wait_for("up", T);
    let dials = remote.connections();
    net.set_up(&[]);
    let pinged = net.pinged().len();
    remote.cut_link();
    c.wait_for("devbox is not answering — pinging it every 200ms", T);
    until("three pings after the drop", || {
        net.pinged().len() >= pinged + 3
    });
    assert_eq!(
        remote.connections(),
        dials,
        "dialled while the host did not answer"
    );
    net.set_up(&["devbox"]);
    // Resumed: the status line is taken down (keys typed offline are
    // dropped, so type after it).
    c.wait_for("\x1b[23;0t", T);
    c.send(b"back\r");
    // After the Ctrl-L every resume sends first (redraw_on_reconnect).
    c.wait_for("got:\x0cback", T);
    assert_eq!(remote.connections(), dials + 1);
}

#[test]
fn an_unchecked_host_keeps_the_dial_backoff() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(
        "persist: true\naliases:\n  devbox:\n    - host: devbox.lan\n      reachability_check: false\n",
    );
    let mut env = refs(&env);
    env.extend([("ACS_PING_MS", "200"), ("ACS_BACKOFF_MS", "100")]);
    let mut c = Client::start_env(&remote, &session("devbox", &[]), &env);
    c.wait_for("up", T);
    remote.cut_link();
    c.send(b"again\r");
    c.wait_for("got:again", T);
    // It cannot be pinged, so it never was: redialled on the backoff.
    assert!(net.pinged().is_empty(), "{:?}", net.pinged());
    assert!(!c.text().contains("is not answering"), "{:?}", c.text());
}

/// An `ssh` that refuses every connection until `up` exists, then is the
/// fake `ssh` of `ssh`: a host that is down at the first connect.
fn flaky(dir: &TempDir, ssh: &Ssh) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.path().join("flaky-ssh");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n[ -e '{up}' ] || {{ echo 'ssh: connect to host: Connection refused' >&2; exit 255; }}\nexec '{ssh}' \"$@\"\n",
            up = dir.path().join("up").display(),
            ssh = ssh.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn a_first_connect_that_fails_is_waited_out_before_any_session() {
    let remote = Remote::installed();
    let ssh = Ssh::new(&[("devbox", &remote)]);
    let dir = TempDir::new();
    let flaky = flaky(&dir, &ssh);
    let net = Net::new(&["devbox"]);
    let env = net.env("reachability_interval: 200ms\n");
    let path = flaky.display().to_string();
    let mut args = vec!["--persist", "--ssh", &path];
    args.extend(["devbox", "s", "--", "/bin/sh", "-c", "echo up; sleep 30"]);
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    // The dial fails; the host answers pings but its ssh is not up yet, so
    // each answer is followed by a dial that fails again — not an exit.
    c.wait_for(
        "devbox is not answering — pinging it every 200ms (Ctrl-C gives up)",
        T,
    );
    net.set_up(&[]);
    let pinged = net.pinged().len();
    until("pings while down", || net.pinged().len() >= pinged + 2);
    std::fs::write(dir.path().join("up"), "").unwrap();
    net.set_up(&["devbox"]);
    c.wait_for("new session 's' on devbox", T);
    c.wait_for("up", T);
}
