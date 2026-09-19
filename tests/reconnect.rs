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
    // The resume's Ctrl-L comes first (DESIGN §5.2).
    c.wait_for("got:\x0cafter", T);
    let text = c.text();
    assert_eq!(text.matches("got:hello").count(), 1, "{text}");
}

#[test]
fn no_reconnect_exits_on_a_drop() {
    let remote = Remote::installed();
    // No short dead-link timeout: the cut is seen at once, and on a loaded
    // machine a short one could end the first connection before it is up.
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
        &[],
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

#[test]
fn a_network_change_redials_at_once() {
    let remote = Remote::installed();
    let fifo = remote.root.path().join("netchange");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let fifo_s = fifo.to_str().unwrap().to_string();
    let mut c = start(
        &remote,
        "net",
        TICKER,
        &[("ACS_BACKOFF_MS", "20000"), ("ACS_NETWATCH_FIFO", &fifo_s)],
    );
    c.wait_for("#10#", T);
    remote.cut_link();
    c.wait_for("reconnecting in 20s", T);
    let t0 = Instant::now();
    // Wi-Fi came back: the watcher fires, the client redials now.
    let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    std::io::Write::write_all(&mut w, b"up").unwrap();
    remote.wait_connections(2, Duration::from_secs(5));
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let target = last_number(&c) + 20;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
}

// ---- bug hunt 2026-09-18 -----------------------------------------------------

/// Regression (acs-znr): a host that accepts the connection and then says
/// nothing — before its marker, or after it, instead of WELCOME — is given
/// up on in time rather than waited for forever.
#[test]
fn a_silent_host_is_given_up_on() {
    let ready = acs::proto::ready_line();
    for said in ["", ready.as_str()] {
        let remote = Remote::installed();
        remote.silence(Some(said));
        let mut c = start(
            &remote,
            "sl",
            "echo up; sleep 30",
            &[("ACS_DIAL_TIMEOUT_MS", "500")],
        );
        let t0 = Instant::now();
        assert_eq!(c.wait(T), 255, "{said:?}: {}", c.text());
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        c.wait_for("no answer from devbox within 0.5 s", T);
    }
}

/// Regression (acs-znr): a redial into a host that has gone quiet times out
/// and returns to the backoff wait — and reconnects once the host answers.
#[test]
fn a_redial_into_a_silent_host_returns_to_the_backoff() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    // The limit covers the first connection too, which on a loaded test
    // machine can take a few seconds.
    env.push(("ACS_DIAL_TIMEOUT_MS", "4000"));
    let mut c = start(&remote, "rs", "echo up; cat", &env);
    c.wait_for("up", T);
    remote.silence(Some(""));
    remote.cut_link();
    // Redials time out one after another instead of one hanging.
    remote.wait_connections(3, Duration::from_secs(30));
    remote.silence(None);
    // Resumed: the status line's title is popped, and keys go through.
    c.wait_for("\x1b[23;0t", T);
    c.send(b"hello\r");
    c.wait_for("hello", T);
}

/// Regression (acs-qrn): when the session ends while the link is down, the
/// status line and the title pushed for it are taken back.
#[test]
fn a_session_ending_during_an_outage_leaves_no_status_behind() {
    let remote = Remote::installed();
    let flag = remote.root.path().join("end");
    let cmd = format!(
        "echo up; while [ ! -f '{}' ]; do sleep 0.05; done; exit 3",
        flag.display()
    );
    let mut c = start(
        &remote,
        "se",
        &cmd,
        &[
            ("ACS_BACKOFF_MS", "1500"),
            ("ACS_PING_MS", "200"),
            ("ACS_DEAD_MS", "800"),
        ],
    );
    c.wait_for("up", T);
    remote.cut_link();
    c.wait_for("reconnecting in", T);
    // The program ends while nobody is attached.
    std::fs::write(&flag, "").unwrap();
    assert_eq!(c.wait(T), 4, "{}", c.text());
    c.wait_for("the session has ended", T);
    let text = c.text();
    let pushed = text.rfind("\x1b[22;0t").expect("a title was pushed");
    assert!(
        text[pushed..].contains("\x1b[23;0t"),
        "the title was not popped: {:?}",
        &text[pushed..]
    );
    assert!(c.echo_on());
}

#[test]
fn the_bell_rings_while_the_link_is_down_too() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "ob",
        "echo up; sleep 30",
        &[("ACS_BACKOFF_MS", "20000")],
    );
    c.wait_for("up", T);
    remote.cut_link();
    // The end of the status line (its title has a BEL of its own).
    c.wait_for("d to detach)\x1b[0m\x1b8", T);
    c.send(&[0x1d]);
    std::thread::sleep(Duration::from_millis(50));
    c.send(&[0x1d]);
    c.wait_for("\x07", T);
    c.send(b"d");
    assert_eq!(c.wait(T), 0);
}

/// Regression (acs-wxa): the command-key window set with
/// `ACS_ESCAPE_TIMEOUT_MS` also applies while the link is down.
#[test]
fn the_escape_window_is_the_same_offline() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "ew",
        "echo up; sleep 30",
        &[
            ("ACS_BACKOFF_MS", "20000"),
            ("ACS_ESCAPE_TIMEOUT_MS", "3000"),
        ],
    );
    c.wait_for("up", T);
    remote.cut_link();
    c.wait_for("reconnecting in 20s", T);
    // Well past the default 400 ms window, inside the configured one.
    c.send(&[0x1d]);
    std::thread::sleep(Duration::from_millis(1000));
    c.send(&[0x1d, b'd']);
    assert_eq!(c.wait(T), 0, "{}", c.text());
    c.wait_for("detached from devbox/ew", T);
}
