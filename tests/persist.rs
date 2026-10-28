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
    wait_until(T, || net.pinged().len() >= 3, || "never: three pings");
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
    wait_until(
        T,
        || net.pinged().len() >= pinged + 3,
        || "never: three pings after the drop",
    );
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

/// acs-vxg: a lost host is pinged every second (here `ACS_PERSIST_FAST_MS`,
/// 100 ms) for the first 30 of them, and only then every
/// `reachability_interval` — before, it waited the whole 10 s for the first.
#[test]
fn a_lost_host_is_pinged_fast_at_first_then_every_interval() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox"]);
    let env = net.env("persist: true\nreachability_interval: 10s\n");
    let mut env = refs(&env);
    env.extend([("ACS_PING_MS", "200"), ("ACS_PERSIST_FAST_MS", "100")]);
    let mut c = Client::start_env(&remote, &session("devbox", &[]), &env);
    c.wait_for("up", T);
    net.set_up(&[]);
    let pinged = net.pinged().len();
    remote.cut_link();
    c.wait_for(
        "devbox is not answering — pinging it every 100ms at first, then 10s",
        T,
    );
    let lost = Instant::now();
    // Fast at first: five pings well inside the first 10 s interval.
    let fast = Duration::from_secs(2);
    wait_until(
        fast.saturating_sub(lost.elapsed()),
        || net.pinged().len() >= pinged + 5,
        || "not pinged fast after the loss",
    );
    // Then backed off: after the 3 s of fast pings, none for a while.
    std::thread::sleep(Duration::from_millis(3500).saturating_sub(lost.elapsed()));
    let settled = net.pinged().len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(net.pinged().len(), settled, "still pinging fast after 3 s");
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
    wait_until(
        T,
        || net.pinged().len() >= pinged + 2,
        || "never: pings while down",
    );
    std::fs::write(dir.path().join("up"), "").unwrap();
    net.set_up(&["devbox"]);
    c.wait_for("new session 's' on devbox", T);
    c.wait_for("up", T);
}

/// acs-ysx: every ping step shows its status, and while the host stays
/// down the text is the same each time. A line already up is not drawn
/// again for nothing: a long outage, when the link is at its worst, does
/// not send the whole span on every step.
#[test]
fn an_unchanged_status_line_is_drawn_once_across_ping_steps() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox"]);
    let env = net.env("persist: true\nreachability_interval: 100ms\n");
    let mut env = refs(&env);
    env.push(("ACS_PING_MS", "100"));
    let mut c = Client::start_env(&remote, &session("devbox", &[]), &env);
    c.wait_for("up", T);
    net.set_up(&[]);
    let pinged = net.pinged().len();
    remote.cut_link();
    let line = "\x1b[7macs: devbox is not answering — pinging it every 100ms";
    c.wait_for(line, T);
    wait_until(
        T,
        || net.pinged().len() >= pinged + 4,
        || "never: four pings after the drop",
    );
    let drawn = c.text().matches(line).count();
    assert_eq!(drawn, 1, "the same status line was drawn {drawn} times");
    // And it still comes down when the host is back.
    net.set_up(&["devbox"]);
    c.wait_for("\x1b[23;0t", T);
}
