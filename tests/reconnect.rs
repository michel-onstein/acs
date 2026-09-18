//! Surviving network interruptions (acs-5v9.16, DESIGN §5.2–§5.4): the link
//! is cut or frozen under a running program and the client resumes without
//! losing or repeating a byte.

mod common;

use std::time::{Duration, Instant};

use common::*;

/// Fast timers so a test takes seconds, not minutes.
const FAST: &[(&str, &str)] = &[
    ("ACS_BACKOFF_MS", "100"),
    ("ACS_PING_MS", "200"),
    ("ACS_DEAD_MS", "800"),
];

const TICKER: &str = "i=0; while true; do i=$((i+1)); printf '#%d#\\n' $i; sleep 0.01; done";

fn start(remote: &Remote, session: &str, cmd: &str, env: &[(&str, &str)]) -> Client {
    Client::start_env(
        remote,
        &["devbox", session, "--", "/bin/sh", "-c", cmd],
        env,
    )
}

/// The `#n#` numbers in the client's output, in order. A status line may
/// land in the middle of a number, so its save/restore-cursor span goes.
fn numbers(text: &str) -> Vec<u64> {
    let mut clean = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("\x1b7") {
        clean.push_str(&rest[..i]);
        rest = match rest[i..].find("\x1b8") {
            Some(j) => &rest[i + j + 2..],
            None => "",
        };
    }
    clean.push_str(rest);
    clean.split('#').filter_map(|p| p.parse().ok()).collect()
}

fn assert_consecutive(text: &str) {
    let n = numbers(text);
    assert!(n.len() > 10, "too little output: {n:?}");
    for w in n.windows(2) {
        assert_eq!(
            w[1],
            w[0] + 1,
            "lost or repeated output around {} → {}",
            w[0],
            w[1]
        );
    }
}

fn last_number(c: &Client) -> u64 {
    *numbers(&c.text()).last().unwrap_or(&0)
}

#[test]
fn cut_link_mid_stream_resumes_without_loss() {
    let remote = Remote::installed();
    let mut c = start(&remote, "cut", TICKER, FAST);
    c.wait_for("#20#", T);
    remote.cut_link();
    remote.wait_connections(2, T);
    let target = last_number(&c) + 100;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
    assert!(c.text().contains("reconnecting"), "status line shown");
}

#[test]
fn frozen_link_is_declared_dead_and_replaced() {
    let remote = Remote::installed();
    let mut c = start(&remote, "frz", TICKER, FAST);
    c.wait_for("#20#", T);
    // Freeze the connection: nothing flows, nothing closes.
    let pid = *remote.transport_pids().last().unwrap();
    acs::sys::kill(pid, libc::SIGSTOP).unwrap();
    let t0 = Instant::now();
    remote.wait_connections(2, T);
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "declared dead too early"
    );
    let _ = acs::sys::kill(pid, libc::SIGKILL);
    let target = last_number(&c) + 100;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
}

#[test]
fn input_sent_around_a_drop_arrives_exactly_once() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "once",
        "echo ready; while read l; do echo got:$l; done",
        FAST,
    );
    c.wait_for("ready", T);
    c.send(b"hello\r");
    remote.cut_link();
    remote.wait_connections(2, T);
    c.send(b"after\r");
    c.wait_for("got:after", T);
    let text = c.text();
    assert_eq!(text.matches("got:hello").count(), 1, "{text}");
}

#[test]
fn no_reconnect_exits_on_a_drop() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    env.push(("ACS_BACKOFF_MS", "100"));
    let mut c = Client::start_env(
        &remote,
        &[
            "devbox",
            "nr",
            "--no-reconnect",
            "--",
            "/bin/sh",
            "-c",
            "echo up; sleep 30",
        ],
        &env,
    );
    c.wait_for("up", T);
    remote.cut_link();
    assert_eq!(c.wait(T), 255);
    c.wait_for(
        "connection lost — the session keeps running; reattach with: acs devbox nr",
        T,
    );
    assert!(c.echo_on());
    assert!(remote.session_exists("nr"));
}

#[test]
fn detach_works_while_the_link_is_down() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "dd",
        "echo up; sleep 30",
        &[("ACS_BACKOFF_MS", "20000")],
    );
    c.wait_for("up", T);
    remote.cut_link();
    c.wait_for("reconnecting in 20s", T);
    let t0 = Instant::now();
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    assert!(t0.elapsed() < Duration::from_secs(5));
    c.wait_for("detached from devbox/dd", T);
    assert!(c.echo_on());
    // The title pushed for the status line was popped again.
    assert!(c.text().contains("\x1b[23;0t"));
}

#[test]
fn exit_needs_the_link() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "xd",
        "echo up; sleep 30",
        &[("ACS_BACKOFF_MS", "20000")],
    );
    c.wait_for("up", T);
    remote.cut_link();
    c.wait_for("reconnecting", T);
    c.send(&command(b'x'));
    c.wait_for("ending the session needs the connection", T);
    assert!(c.child.try_wait().unwrap().is_none(), "client must stay");
}

#[test]
fn output_lost_to_a_small_ring_is_a_gap_and_redraw() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    env.push(("ACS_RING", "4096"));
    env.push(("ACS_BACKOFF_MS", "700"));
    let mut c = start(
        &remote,
        "gap",
        "echo up; while true; do head -c 2000 /dev/zero | tr '\\0' y; echo; sleep 0.01; done",
        &env,
    );
    c.wait_for("up", T);
    let before = c.output().len();
    remote.cut_link();
    remote.wait_connections(2, T);
    // After reconnecting with an overwritten offset the client clears the
    // screen for the program's redraw.
    let deadline = Instant::now() + T;
    loop {
        let out = c.output();
        if out[before..].windows(6).any(|w| w == b"\x1b[H\x1b[J") {
            break;
        }
        assert!(Instant::now() < deadline, "no clear after the gap");
        std::thread::sleep(Duration::from_millis(20));
    }
}
