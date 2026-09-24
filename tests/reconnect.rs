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

/// The dead interval `frozen_link_is_declared_dead_and_replaced` runs with,
/// in place of `FAST`'s: long enough that the bound it reads against has
/// room, and the only test here that waits out a whole one.
const DEAD: Duration = Duration::from_secs(4);

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

/// acs-iyq: the first redial after a drop waits for nothing, so a momentary
/// drop costs the dial and no more.
///
/// The backoff is two minutes — longer than every deadline in the suite, as
/// acs-7wu left it — so a client that waited even its *first* step could not
/// be back inside `T` however fast the machine is. Nothing here is timed:
/// resuming at all is the assertion, and the clock it is read against is the
/// helper's own deadline (acs-o8h).
#[test]
fn the_first_redial_after_a_drop_does_not_wait() {
    let remote = Remote::installed();
    let mut c = start(&remote, "now", TICKER, &[("ACS_BACKOFF_MS", "120000")]);
    c.wait_for("#20#", T);
    remote.cut_link();
    c.wait_resumed();
    let target = last_number(&c) + 20;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
    // The status line still says what happened — it is what the resume
    // repaints over — but it names no wait, because there was none.
    let text = c.text();
    assert!(text.contains("reconnecting now"), "{text:?}");
    assert!(!text.contains("reconnecting in"), "{text:?}");
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

/// Regression (acs-ode): under steady output the client hears the master
/// and never pings on its own, so only its PONG to the master's PING keeps
/// the master from giving it up.
///
/// What is waited for is the answers, not a stretch of wall clock with the
/// link still standing (acs-o6x). The old shape slept past a few of the
/// master's ping intervals and then counted connections, which asks the
/// host as much as it asks acs: descheduling either process for longer
/// than `ACS_DEAD_MS` — 800 ms here — is a dropped link and a red gate for
/// a client that was never at fault, and on a busy laptop that happens.
/// The master's count restarts with each connection, so eight answers in a
/// row are eight liveness rounds *one* link came through: a client that
/// stops answering never gets there (nothing is logged at all), nor does a
/// master that stops counting the answers as having heard it — it gives
/// the link up after three. A host that stalls only makes it take longer.
#[test]
fn a_client_busy_with_output_answers_the_masters_ping() {
    let remote = Remote::installed();
    remote.log_master();
    let mut c = start(&remote, "busy", TICKER, FAST);
    c.wait_for("#20#", T);
    c.wait_until(
        "the client has answered eight of the master's pings on one link",
        |_| remote.master_log().contains("answered ping 8 on this link"),
        T,
    );
}

/// A link that goes quiet without closing is replaced — after the dead
/// interval, not the moment it goes quiet.
///
/// "Not the moment" is a *lower* bound on elapsed time, and a lower bound is
/// the one direction a loaded machine cannot break: starving the client only
/// makes the redial later. What it can break is the anchor. The client's
/// clock runs from the last byte it heard, not from the `SIGSTOP`, so a
/// client that had been starved for most of the interval *before* the freeze
/// declares the link dead almost at once and is right to. So the room
/// between the bound and the interval is what has to be generous (acs-7wu,
/// acs-o8h): the interval is four seconds and the bound one of them, where
/// it used to be 800 ms and 500 — the same statement with ten times the
/// slack. For this to fire without the bug, the client would have to have
/// heard nothing for three seconds while the link was still up, the ticker
/// printing every 10 ms and its own PING going out every 200.
#[test]
fn frozen_link_is_declared_dead_and_replaced() {
    let remote = Remote::installed();
    // The master keeps its own liveness (acs-ode); a minute of it, so the
    // one judgement on trial here is the client's.
    remote.remote_env(&[("ACS_DEAD_MS", "60000")]);
    let mut env: Vec<(String, String)> = FAST
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.push(("ACS_DEAD_MS".to_string(), DEAD.as_millis().to_string()));
    let mut c = start(&remote, "frz", TICKER, &refs(&env));
    c.wait_for("#20#", T);
    // Counted from here, not from one: a remote that has to install acs
    // first spends a connection or two before the session (acs-cu8).
    let dialled = remote.transport_pids().len();
    // Freeze the connection: nothing flows, nothing closes.
    let pid = *remote.transport_pids().last().unwrap();
    acs::sys::kill(pid, libc::SIGSTOP).unwrap();
    let t0 = Instant::now();
    remote.wait_connections(dialled + 1, T);
    assert!(t0.elapsed() >= DEAD / 4, "declared dead too early");
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
    c.wait_resumed();
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

/// Regression (acs-7k7): a terminal that stops reading for longer than the
/// dead interval is not a lost link. The client blocks writing to stdout
/// meanwhile, and judged liveness by what it had decoded before the block
/// rather than by the bytes waiting in the pipe.
///
/// The 8 s pause is the stimulus and stays a sleep: it must *exceed*
/// `ACS_DEAD_MS` (800 ms), and a starved host can only make it longer. What
/// followed it was a 500 ms settle before a negative assertion, which a
/// stalled host silently turns into no test at all (acs-7wu). The settle is
/// now an event: the session prints `RESUMED` only once the flag file is
/// there, and the flag is written after the terminal starts draining again,
/// so those bytes cannot be among the ones buffered during the pause — they
/// were produced afterwards and carried across the same link. Seeing them is
/// the client saying it read the host after the write unblocked and lived,
/// which is exactly the moment the bug fired: the first `tick` after the
/// unblocking write, with the newly read bytes not yet decoded. Waiting for
/// output alone would not do — on unpause the flood of buffered `y`s arrives
/// whether the link was kept or dropped and redialled.
///
/// The stimulus itself is untouched: the flag test is a shell builtin beside
/// two forks already in the loop, and until the flag appears the frame is the
/// same 101 bytes, so the trickle that fills the terminal's buffer mid-pause
/// is the one acs-7k7 was reproduced with.
#[test]
fn a_stalled_terminal_is_not_a_dead_link() {
    let remote = Remote::installed();
    // The master keeps its client for a minute (it has its own liveness,
    // acs-ode); this is about the client's judgement of the master.
    remote.remote_env(&[("ACS_DEAD_MS", "60000"), ("ACS_PING_MS", "20000")]);
    let go = remote.root.path().join("go");
    let mut c = start(
        &remote,
        "stall",
        // A trickle: one small frame per read, and the terminal's buffer
        // fills partway through the pause, blocking the client in a write
        // with nothing left to decode.
        &format!(
            "echo up; while true; do head -c 100 /dev/zero | tr '\\0' y; \
             [ -f '{}' ] && printf RESUMED; echo; sleep 0.05; done",
            go.display()
        ),
        FAST,
    );
    c.wait_for("up", T);
    // Counted from here, not from one: a remote that has to install acs
    // first spends a connection or two before the session (acs-cu8), and
    // what this test is about is whether the stall costs one *more*.
    let dialled = remote.transport_pids().len();
    // Nothing is read for well past ACS_DEAD_MS (800 ms).
    c.set_paused(true);
    std::thread::sleep(Duration::from_secs(8));
    c.set_paused(false);
    // Only now, so every RESUMED is output made after the stall.
    std::fs::write(&go, "").unwrap();
    c.wait_for("RESUMED", T);
    assert_eq!(
        remote.transport_pids().len(),
        dialled,
        "the link was given up on"
    );
    assert!(!c.text().contains("reconnecting"), "{}", c.text());
}

/// The detach key is honoured during the offline wait, rather than noticed
/// when the wait expires — which is what the elapsed bound distinguishes, so
/// it stays a bound on elapsed time (acs-7wu).
///
/// What changed is the room it has: two minutes of backoff against the
/// suite's own `T`, where it was 20 s against 5 s. The same statement — an
/// exit a quarter of the way into the wait cannot be the wait ending — with
/// six times the slack. `T` rather than a number of its own is the point:
/// the backoff now outlasts every deadline in the suite, so a detach that
/// waited for it is caught by the `c.wait(T)` above as well, and this bound
/// is the one thing in the test that cannot be starved past first. acs-o8h
/// made the same move in `the_escape_window_is_the_same_offline`.
#[test]
fn detach_works_while_the_link_is_down() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "dd",
        "echo up; sleep 30",
        &[("ACS_BACKOFF_MS", "120000")],
    );
    c.wait_for("up", T);
    // The network is down, not merely the link: the free first redial
    // (acs-iyq) is refused, and the client is in its wait once it says so.
    remote.refuse_one_dial();
    remote.cut_link();
    c.wait_for("reconnecting in 120s", T);
    let t0 = Instant::now();
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    assert!(t0.elapsed() < T, "{:?}", t0.elapsed());
    c.wait_for("detached from devbox/dd", T);
    assert!(c.echo_on());
    // The title pushed for the status line was popped again.
    assert!(c.text().contains("\x1b[23;0t"));
}

/// Regression (acs-qty): keys typed while ssh redials (in cooked mode, for
/// its prompts) are dropped, not delivered to the program on WELCOME.
///
/// Nothing here is timed (acs-o8h). The redial is *held* short of the
/// remote command rather than merely made slow, and the key goes in once
/// the client itself says it has left raw mode for the dial — so the key
/// cannot land outside the redial however starved the host is, where the
/// old shape gave it a 1.5 s window it had already spent 300 ms of.
#[test]
fn keys_typed_during_a_redial_are_dropped() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "typed",
        "echo ready; while read l; do echo got:$l; done",
        FAST,
    );
    c.wait_for("ready", T);
    remote.hold_dial();
    remote.cut_link();
    remote.wait_connections(2, T);
    // The redial is under way and stays that way until it is released.
    c.wait_until("the client has left raw mode to redial", |c| c.echo_on(), T);
    c.send(b"typed\r");
    remote.release_dial();
    // The status line goes when the resume's WELCOME arrives.
    c.wait_for("\x1b[23;0t", T);
    c.send(b"after\r");
    c.wait_for("got:\x0cafter", T);
    let text = c.text();
    assert!(!text.contains("got:typed"), "{text:?}");
}

/// Regression (acs-qty): a Ctrl-C while ssh redials ends the client with the
/// status line taken away and the title popped.
#[test]
fn ctrl_c_during_a_redial_clears_the_status_line() {
    let remote = Remote::installed();
    let mut c = start(&remote, "cc", "echo ready; sleep 30", FAST);
    c.wait_for("ready", T);
    remote.slow_dial(Some("5"));
    remote.cut_link();
    c.wait_for("\x1b[22;0t", T);
    remote.wait_connections(2, T);
    // Ctrl-C is only the interrupt once the client has left raw mode for
    // the dial. Waiting for that rather than sleeping on it: 300 ms is
    // plenty on an idle machine and not always enough on a loaded one
    // (acs-kip).
    c.wait_until("the client is in cooked mode", |c| c.echo_on(), T);
    c.send(b"\x03");
    c.wait(T);
    let text = c.text();
    let tail = &text[text.rfind("\x1b[22;0t").unwrap()..];
    assert!(tail.contains("\x1b[23;0t"), "title not popped: {tail:?}");
    assert!(c.echo_on());
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
    remote.refuse_one_dial();
    remote.cut_link();
    // The wait, not the free first attempt (acs-iyq): `in 20s` is the
    // second attempt's line, and command keys are only read in the wait.
    c.wait_for("reconnecting in 20s", T);
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
    // The 700 ms backoff is what lets the program overwrite the 4 KiB ring
    // before the client is back; the free first attempt (acs-iyq) would
    // skip it, so the network refuses that one.
    remote.refuse_one_dial();
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

/// Regression (acs-xk4): the modes a program turned on before a gap are
/// still reset on a later detach — the gap clears the screen, not what the
/// client knows of the terminal.
#[test]
fn modes_on_before_a_gap_are_reset_on_detach() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    env.push(("ACS_RING", "4096"));
    env.push(("ACS_BACKOFF_MS", "700"));
    let mut c = start(
        &remote,
        "gapmodes",
        "printf '\\033[?1049h\\033[?1000;1006h\\033[?2004hup\\n'; \
         while true; do head -c 2000 /dev/zero | tr '\\0' y; echo; sleep 0.01; done",
        &env,
    );
    c.wait_for("up", T);
    // As above: the backoff is what lets the ring be overwritten, so the
    // free first attempt is refused out of the way (acs-iyq).
    remote.refuse_one_dial();
    remote.cut_link();
    remote.wait_connections(2, T);
    c.wait_for("\x1b[H\x1b[J", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let out = c.text();
    let tail = &out[out.rfind("\x1b[H\x1b[J").unwrap()..];
    for reset in ["\x1b[?1000l", "\x1b[?1006l", "\x1b[?2004l", "\x1b[?1049l"] {
        assert!(tail.contains(reset), "missing {reset:?} after the gap");
    }
}

/// Regression (acs-xk4): when the session is another program by the time the
/// client is back, the old program's modes are reset before it is forgotten.
#[test]
fn modes_of_a_restarted_session_are_reset() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    env.push(("ACS_BACKOFF_MS", "3000"));
    let mut a = start(
        &remote,
        "restart",
        "printf '\\033[?1049h\\033[?1000;1006hTUI'; sleep 1",
        &env,
    );
    a.wait_for("TUI", T);
    // The free first redial (acs-iyq) would land while the first program is
    // still running and resume it; refused, it costs nothing and `a` is
    // left in the backoff, as it was before there was a free attempt.
    remote.refuse_one_dial();
    remote.cut_link();
    // The first program ends while the link is down; another takes its name.
    let deadline = Instant::now() + T;
    while remote.session_exists("restart") {
        assert!(Instant::now() < deadline, "the first program did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut b = start(&remote, "restart", "echo second; sleep 30", &[]);
    b.wait_for("second", T);
    b.send(&command(b'd'));
    assert_eq!(b.wait(T), 0);
    a.wait_for("the session was restarted", T);
    let out = a.text();
    let tail = &out[out.rfind("the session was restarted").unwrap()..];
    let clear = tail.find("\x1b[H\x1b[J").expect("no clear");
    for reset in ["\x1b[?1000l", "\x1b[?1006l", "\x1b[?1049l"] {
        let at = tail
            .find(reset)
            .unwrap_or_else(|| panic!("missing {reset:?}"));
        assert!(at < clear, "{reset:?} after the clear");
    }
}

/// A network change redials out of the offline wait instead of sitting it
/// out — again a statement about elapsed time, and again with two minutes of
/// backoff against `T` rather than 20 s against 5 s (acs-7wu).
///
/// The change is a real one: the machine is on different networks than it
/// was when the client started, which is what makes the kernel's hint a
/// change rather than a mention (acs-6p8, below).
#[test]
fn a_network_change_redials_at_once() {
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let mut env = vec![("ACS_BACKOFF_MS".to_string(), "120000".to_string())];
    env.extend(watch.env());
    let mut c = start(&remote, "net", TICKER, &refs(&env));
    c.wait_for("#10#", T);
    // The network is down: the free first redial (acs-iyq) is refused, and
    // the two minutes of backoff behind it are what the watcher then has to
    // cut short for this to mean anything.
    remote.refuse_one_dial();
    remote.cut_link();
    c.wait_for("reconnecting in 120s", T);
    let t0 = Instant::now();
    // Wi-Fi came back on another network: the watcher fires, and this
    // machine's own addresses are not the ones it had, so the client
    // redials now.
    watch.set_networks("10.0.0.5/24\n");
    watch.hint();
    remote.wait_connections(2, T);
    assert!(t0.elapsed() < T, "{:?}", t0.elapsed());
    let target = last_number(&c) + 20;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
}

/// Regression (acs-6p8): the kernel *mentioning* the network is not the
/// network changing.
///
/// The watcher used to report every address and interface message as a
/// change, and the first one of a wait was always taken. A laptop emits
/// those the whole time — an unrelated interface appearing, a VPN route
/// churning, an interface flapping while the link's own path is untouched
/// — so a client sitting out an outage redialled over and over, and every
/// key typed at one of those moments was discarded on the `WELCOME` that
/// followed (DESIGN §5.2). That is what made three tests in this file fail
/// together on a cold full-parallel run with two minutes of backoff
/// configured, and on a real laptop it is the promise of §5.2 broken:
/// `d` typed during an outage does nothing at all.
///
/// Nothing here is timed, and nothing waits for a laptop to roam. The hint
/// goes in before the keys, and the client examines the watcher ahead of
/// stdin in the same pass of its poll — a descriptor stays readable — so
/// the detach cannot overtake the hint however starved the machine is. A
/// wait cut short by it puts the client in a redial, where what was typed
/// is flushed rather than read, and then it never detaches.
#[test]
fn a_hint_that_changed_no_network_does_not_cut_the_wait_short() {
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let mut env = vec![("ACS_BACKOFF_MS".to_string(), "120000".to_string())];
    env.extend(watch.env());
    // The program outlasts `T`, so exiting 0 can only be the detach and
    // not the session ending under a client that redialled and resumed.
    let mut c = start(&remote, "nk", "echo up; sleep 60", &refs(&env));
    c.wait_for("up", T);
    remote.refuse_one_dial();
    remote.cut_link();
    c.wait_for("reconnecting in 120s", T);
    // Counted from here, not from one: getting this far can take a
    // connection or two of its own — a remote that has to install acs
    // first spends them before the session.
    let dialled = remote.transport_pids().len();
    // The kernel says something about the network, and this machine is on
    // exactly the networks it was on: nothing to redial for.
    watch.hint();
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0, "{}", c.text());
    c.wait_for("detached from devbox/nk", T);
    assert_eq!(
        remote.transport_pids().len(),
        dialled,
        "the wait was cut short by a hint that changed nothing: {}",
        c.text()
    );
}

/// acs-ft1: the watcher is polled while the link is *up* too, and what a
/// change buys there is a question, not a redial.
///
/// The dead-link timeout is a minute here, so nothing but the network
/// change can end this link inside `T` — and the client's own clock cannot
/// reach it either, which is exactly the laptop-wake case the watcher is
/// for (`sys::now_ms` is monotonic and stops with the machine, so the sleep
/// is not silence anything measured). The connection is frozen rather than
/// cut, as a Wi-Fi switch leaves it: nothing flows and nothing closes, so
/// there is no EOF to notice. `ACS_NETCHECK_MS` is left at its default, so
/// what is on trial is the deadline acs ships with.
#[test]
fn a_network_change_ends_a_frozen_link_without_the_dead_timeout() {
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let mut env = vec![
        ("ACS_BACKOFF_MS".to_string(), "100".to_string()),
        ("ACS_PING_MS".to_string(), "20000".to_string()),
        ("ACS_DEAD_MS".to_string(), "60000".to_string()),
    ];
    env.extend(watch.env());
    let mut c = start(&remote, "wake", TICKER, &refs(&env));
    c.wait_for("#20#", T);
    let dialled = remote.transport_pids().len();
    let pid = *remote.transport_pids().last().unwrap();
    acs::sys::kill(pid, libc::SIGSTOP).unwrap();
    // Woken on another network: the kernel's hint, and addresses that are
    // not the ones this machine had.
    watch.set_networks("10.0.0.5/24\n");
    watch.hint();
    // Inside `T`, which is half the dead interval: only the change can
    // have ended it.
    remote.wait_connections(dialled + 1, T);
    let _ = acs::sys::kill(pid, libc::SIGKILL);
    let target = last_number(&c) + 100;
    c.wait_for(&format!("#{target}#"), T);
    assert_consecutive(&c.text());
}

/// acs-ft1, the other half: a network change under a link that is *still
/// working* costs nothing — no redial, and so not a byte of what was typed
/// (keys typed into a redial are discarded, DESIGN §5.2). Dropping a live
/// session because an address moved would be the worse bug of the two.
///
/// Nothing is timed (acs-o8h). The program's own output is the clock: the
/// ticker sleeps 10 ms between lines, so a hundred more of them cannot
/// have taken less than a second — twice the 500 ms the change gives the
/// host here — and the connection count after them is the assertion.
#[test]
fn a_network_change_on_a_live_link_costs_nothing() {
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let mut env: Vec<(String, String)> = FAST
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.push(("ACS_NETCHECK_MS".to_string(), "500".to_string()));
    env.extend(watch.env());
    let mut c = start(&remote, "keep", TICKER, &refs(&env));
    c.wait_for("#20#", T);
    let dialled = remote.transport_pids().len();
    // A VPN came up: the addresses moved, and the link over the one that
    // did not is untouched.
    watch.set_networks("192.168.1.5/24\n10.8.0.2/24\n");
    watch.hint();
    let target = last_number(&c) + 100;
    c.wait_for(&format!("#{target}#"), T);
    assert_eq!(
        remote.transport_pids().len(),
        dialled,
        "a live link was redialled for a network change: {}",
        c.text()
    );
    assert_consecutive(&c.text());
}

// ---- bug hunt 2026-09-18 -----------------------------------------------------

/// Regression (acs-znr): a host that accepts the connection and then says
/// nothing — before its marker, or after it, instead of WELCOME — is given
/// up on in time rather than waited for forever.
///
/// The clock is the host's, so the test does not read it (acs-o8h). A
/// silenced connection holds the link open for a minute, well past the `T`
/// the helpers wait to, so "waited for forever" is a client that never
/// exits — and both give-up messages are formatted from the very `Duration`
/// that was used as the deadline, so the line naming 0.5 s *is* the
/// assertion that the configured limit was the one applied. An `elapsed() <
/// 5 s` around all of it added nothing to either and asked the host to have
/// started a process, dialled and exited inside five seconds.
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
        c.wait_for("no answer from devbox within 0.5 s", T);
        assert_eq!(c.wait(T), 255, "{said:?}: {}", c.text());
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
    // Refused the free attempt (acs-iyq), the client is in its 1.5 s wait:
    // the program has ended long before it dials again.
    remote.refuse_one_dial();
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
    remote.refuse_one_dial();
    remote.cut_link();
    // The end of the wait's status line (its title has a BEL of its own).
    // `in 20s` and not the free attempt's line (acs-iyq): the double tap
    // below is only read while the client is waiting, not while it dials.
    c.wait_for("in 20s (Ctrl-] Ctrl-] d to detach)\x1b[0m\x1b8", T);
    // One write, so the double tap cannot be split across the escape
    // window by a host that deschedules the client between the two presses
    // (acs-o8h): 50 ms of sleep inside the default 400 ms was the tightest
    // wall-clock gap in this file.
    c.send(&[0x1d, 0x1d]);
    c.wait_for("\x07", T);
    c.send(b"d");
    assert_eq!(c.wait(T), 0);
}

/// Regression (acs-wxa): the command-key window set with
/// `ACS_ESCAPE_TIMEOUT_MS` also applies while the link is down.
///
/// The gap between the presses is the subject here, so it stays — but as a
/// floor rather than as a window (acs-o8h). The second press has to land
/// *more* than the default 400 ms after the first, and `sleep` never
/// returns early, so a starved host can only make the gap longer than the
/// test needs. What the gap bought is then read off the client instead of
/// off the clock: arming command mode rings the bell (DESIGN §6.1), so the
/// BEL is the client saying it still had the first press a second later.
/// The configured window is a whole `T` wide so that the end of the gap the
/// test has no hold over — the two scheduling hops around the second press
/// — has the room every other wait in the suite has. The backoff outlasts
/// the gap so that both presses land in the same offline wait, which is
/// what this test is about; it no longer has to outlast the *window*, now
/// that a redial attempt under a held press keeps it (acs-e80, below).
#[test]
fn the_escape_window_is_the_same_offline() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "ew",
        "echo up; sleep 30",
        &[
            ("ACS_BACKOFF_MS", "20000"),
            ("ACS_ESCAPE_TIMEOUT_MS", "30000"),
        ],
    );
    c.wait_for("up", T);
    remote.refuse_one_dial();
    remote.cut_link();
    // Past the end of the wait's status line, whose title carries a BEL of
    // its own — `in 20s`, so both presses land in the offline wait rather
    // than the first one in the free redial (acs-iyq).
    c.wait_for("in 20s (Ctrl-] Ctrl-] d to detach)\x1b[0m\x1b8", T);
    c.send(&[0x1d]);
    // Well past the default 400 ms window, inside the configured one.
    std::thread::sleep(Duration::from_millis(1000));
    c.send(&[0x1d]);
    // The double tap was still a double tap: command mode armed.
    c.wait_for("\x07", T);
    c.send(b"d");
    assert_eq!(c.wait(T), 0, "{}", c.text());
    c.wait_for("detached from devbox/ew", T);
}

/// Regression (acs-e80): the detector lives as long as the client, not as
/// long as a link (DESIGN §6.1), so a command key held when the link dies
/// is still the first press of the double tap after redial attempts have
/// come and gone. `offline()` used to build a fresh one on every entry, and
/// `serve()` one per link, so the press was dropped without a trace.
///
/// The backoff here is `FAST`'s 100 ms against a minute of escape window —
/// the inverse of the constant `the_escape_window_is_the_same_offline`
/// needed before the fix, and the combination the bug is reachable with:
/// someone who widened `ACS_ESCAPE_TIMEOUT_MS` because 400 ms is too quick
/// for them.
///
/// Nothing here is timed. The first press goes in while the client is
/// online and certainly reading stdin, so it cannot miss a window; a third
/// connection is the host saying the second one timed out and the offline
/// wait was re-entered under the held press; and the second press goes in
/// only once the session is back, where there is no deadline to race. The
/// one floor is that the whole round trip fits inside the escape window,
/// and a minute is many times what the nominal few seconds need.
#[test]
fn a_held_command_key_survives_a_redial_attempt() {
    let remote = Remote::installed();
    let mut env = FAST.to_vec();
    env.push(("ACS_ESCAPE_TIMEOUT_MS", "60000"));
    // As in `a_redial_into_a_silent_host_returns_to_the_backoff`: the limit
    // covers the first connection too, which on a loaded test machine can
    // take a few seconds.
    env.push(("ACS_DIAL_TIMEOUT_MS", "4000"));
    let mut c = start(&remote, "hk", "echo up; cat", &env);
    c.wait_for("up", T);
    // Half of Ctrl-] Ctrl-] d, and then the link goes.
    c.send(&[0x1d]);
    remote.silence(Some(""));
    remote.cut_link();
    // A redial attempt expires out of the offline wait, times out against
    // the quiet host, and the wait comes back — all under the held press.
    remote.wait_connections(3, T);
    remote.silence(None);
    c.wait_resumed();
    // Still the first press: this completes the double tap and detaches.
    c.send(&[0x1d, b'd']);
    assert_eq!(c.wait(T), 0, "{}", c.text());
    c.wait_for("detached from devbox/hk", T);
}
