//! The client over a local transport (acs-5v9.15): attach, byte-exact
//! output, detach and re-attach, exit, status codes, terminal restore.

mod common;

use std::time::Duration;

use common::*;

fn sh(cmd: &str) -> Vec<String> {
    vec!["--".into(), "/bin/sh".into(), "-c".into(), cmd.into()]
}

fn start(remote: &Remote, session: &str, cmd: &str) -> Client {
    start_env(remote, session, cmd, &[])
}

fn start_env(remote: &Remote, session: &str, cmd: &str, env: &[(&str, &str)]) -> Client {
    let mut args = vec!["devbox".to_string(), session.to_string()];
    args.extend(sh(cmd));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Client::start_env(remote, &args, env)
}

#[test]
fn output_is_byte_exact_including_escape_sequences() {
    let remote = Remote::installed();
    let data_file = remote.root.path().join("data");
    let data = binary_pattern(&data_file, 300_000);
    let mut c = start(
        &remote,
        "bx",
        &format!(
            "stty raw -echo; printf 'BEGIN'; cat '{}'; printf 'END'; sleep 30",
            data_file.display()
        ),
    );
    c.wait_for("END", T);
    let out = c.output();
    let begin = out.windows(5).position(|w| w == b"BEGIN").unwrap() + 5;
    assert_eq!(&out[begin..begin + data.len()], &data[..], "bytes changed");
    assert_eq!(&out[begin + data.len()..begin + data.len() + 3], b"END");
}

#[test]
fn new_session_is_announced_and_raw_mode_applies() {
    let remote = Remote::installed();
    let mut c = start(&remote, "ann", "echo ready; sleep 30");
    c.wait_for("new session 'ann' on devbox", T);
    c.wait_for("ready", T);
    assert!(!c.echo_on(), "client terminal should be raw");
}

#[test]
fn detach_leaves_the_session_and_reattach_finds_it() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "work",
        "echo started; while read l; do echo got:$l; done",
    );
    c.wait_for("started", T);
    c.send(b"one\r");
    c.wait_for("got:one", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    c.wait_for(
        "detached from devbox/work — reattach with: acs devbox work",
        T,
    );
    assert!(c.echo_on(), "terminal restored after detach");
    assert!(remote.session_exists("work"));

    let mut c2 = Client::start(&remote, &["devbox", "work"]);
    // A fresh attach clears the screen and asks the program to redraw,
    // with a Ctrl-L ahead of what is typed (DESIGN §5.2).
    c2.wait_for("\x1b[H\x1b[J", T);
    c2.send(b"two\r");
    c2.wait_for("got:\x0ctwo", T);
    assert!(!c2.text().contains("new session"));
}

#[test]
fn exit_command_ends_the_session() {
    let remote = Remote::installed();
    let mut c = start(&remote, "gone", "echo started; sleep 1000");
    c.wait_for("started", T);
    c.send(&command(b'x'));
    // The shell dies of SIGHUP: 128 + 1.
    assert_eq!(c.wait(T), 129);
    assert!(c.echo_on());
    let deadline = std::time::Instant::now() + T;
    while remote.session_exists("gone") {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_programs_exit_status_is_the_clients() {
    let remote = Remote::installed();
    let mut c = start(&remote, "st", "echo bye; exit 7");
    c.wait_for("bye", T);
    assert_eq!(c.wait(T), 7);
    assert!(c.echo_on());
}

#[test]
fn single_escape_press_and_other_keys_reach_the_program() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "keys",
        "stty raw -echo; echo armed; od -An -tx1 -N 4",
    );
    c.wait_for("armed", T);
    // Ctrl-] then 'a' goes through at once; Ctrl-] alone after the window.
    c.send(&[0x1d, b'a']);
    std::thread::sleep(Duration::from_millis(50));
    c.send(&[0x1d]);
    std::thread::sleep(Duration::from_millis(600));
    c.send(b"z");
    c.wait_for("7a", T);
    let text = c.text();
    let words: Vec<&str> = text[text.find("armed").unwrap()..]
        .split_whitespace()
        .collect();
    assert!(
        words.windows(4).any(|w| w == ["1d", "61", "1d", "7a"]),
        "{words:?}"
    );
}

#[test]
fn resize_reaches_the_program() {
    let remote = Remote::installed();
    let mut c = start(&remote, "rsz", "echo ready; read x; stty size");
    c.wait_for("ready", T);
    c.resize(101, 33);
    std::thread::sleep(Duration::from_millis(200));
    c.send(b"\r");
    c.wait_for("33 101", T);
}

#[test]
fn modes_left_on_are_reset_on_detach() {
    let remote = Remote::installed();
    let mut c = start(
        &remote,
        "modes",
        "printf '\\033[?1049h\\033[?1000;1006h\\033[?2004hTUI'; sleep 30",
    );
    c.wait_for("TUI", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let out = c.text();
    let tail = &out[out.rfind("TUI").unwrap()..];
    for reset in ["\x1b[?1000l", "\x1b[?1006l", "\x1b[?2004l", "\x1b[?1049l"] {
        assert!(tail.contains(reset), "missing {reset:?} in {tail:?}");
    }
}

#[test]
fn new_flag_creates_numbered_sessions() {
    let remote = Remote::installed();
    let mut a = Client::start(
        &remote,
        &["devbox", "--new", "--", "/bin/sh", "-c", "sleep 30"],
    );
    a.wait_for("new session '1' on devbox", T);
    let mut b = Client::start(
        &remote,
        &["devbox", "--new", "--", "/bin/sh", "-c", "sleep 30"],
    );
    b.wait_for("new session '2' on devbox", T);
}

#[test]
fn unsupported_remote_platform_is_reported() {
    // A missing binary is installed (tests/install.rs); a platform acs has
    // no build for is an error.
    let remote = Remote::new();
    remote.fake_uname("SunOS", "i86pc");
    let mut c = Client::start(&remote, &["devbox"]);
    assert_eq!(c.wait(T), 254);
    c.wait_for("devbox runs SunOS i86pc, which acs does not support", T);
}

#[test]
fn failing_transport_is_unreachable() {
    let remote = Remote::installed();
    let mut c = Client::start_env(&remote, &["--transport-cmd", "false", "devbox"], &[]);
    assert_eq!(c.wait(T), 255);
    c.wait_for("closed before acs started", T);
}

/// Ctrl-] Ctrl-] typed as a person does: two presses, apart.
fn double_tap(c: &Client) {
    c.send(&[0x1d]);
    std::thread::sleep(Duration::from_millis(50));
    c.send(&[0x1d]);
}

#[test]
fn arming_command_mode_rings_the_bell() {
    let remote = Remote::installed();
    let mut c = start(&remote, "bell", "echo started; sleep 30");
    c.wait_for("started", T);
    assert!(!c.output().contains(&0x07));
    double_tap(&c);
    c.wait_for("\x07", T);
    c.send(b"d");
    assert_eq!(c.wait(T), 0);
    c.wait_for("detached from devbox/bell", T);
}

#[test]
fn the_bell_setting_and_its_environment_override() {
    let cfg = acs::testutil::TempDir::new();
    let off = config_env(cfg.path(), "command_bell: false\n");
    let off: Vec<(&str, &str)> = off.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let cases: [(Vec<(&str, &str)>, bool); 3] = [
        (off.clone(), false),
        (vec![("ACS_COMMAND_BELL", "0")], false),
        // The environment wins over the configuration.
        ([&off[..], &[("ACS_COMMAND_BELL", "1")]].concat(), true),
    ];
    for (env, bell) in cases {
        let remote = Remote::installed();
        let mut c = start_env(&remote, "nb", "echo started; sleep 30", &env);
        c.wait_for("started", T);
        double_tap(&c);
        std::thread::sleep(Duration::from_millis(300));
        c.send(b"d");
        assert_eq!(c.wait(T), 0);
        c.wait_for("detached from devbox/nb", T);
        assert_eq!(c.output().contains(&0x07), bell, "{env:?}");
    }
}

#[test]
fn the_bell_waits_for_the_programs_osc_to_end() {
    let remote = Remote::installed();
    // A title the program takes a second to finish, ended by ST.
    let mut c = start(
        &remote,
        "osc",
        "printf 'A\\033]2;ti'; sleep 1; printf 'tle\\033\\\\B'; sleep 30",
    );
    c.wait_for("A\x1b]2;ti", T);
    double_tap(&c);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !c.output().contains(&0x07),
        "a BEL now would end the title: {:?}",
        c.text()
    );
    c.wait_for("B", T);
    // Rung as soon as the title ended, not inside it.
    assert!(
        c.text().contains("A\x1b]2;title\x1b\\\x07B"),
        "{:?}",
        c.text()
    );
}

/// Every `acs:` note the client printed landed where the program's own
/// stream was between sequences and characters, never inside one
/// (acs-z22). The client's mode observer is what "boundary" means, so this
/// uses the same one, fed everything on the terminal that was not a note.
///
/// Returns how many notes there were, so a test can say the interleaving
/// it set up really happened.
fn notes_at_a_boundary(out: &[u8]) -> usize {
    let mut stream = acs::modes::ModeObserver::new();
    let mut notes = 0;
    let mut i = 0;
    while i < out.len() {
        if out[i..].starts_with(b"acs: ") {
            assert!(
                stream.at_boundary(),
                "a note landed inside the program's sequence at byte {i}:\n{}",
                String::from_utf8_lossy(&out[i.saturating_sub(60)..out.len().min(i + 80)])
            );
            notes += 1;
            // Past the line: a note is one line, and anything that looks
            // like a second note inside it is its text.
            i += out[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(out.len() - i, |n| n + 1);
        } else {
            stream.observe(&out[i..i + 1]);
            i += 1;
        }
    }
    notes
}

/// acs-z22 — and the assertion acs-4i2 deliberately left out of its `-v`
/// tests because of it: a note goes where the bell goes, at a boundary of
/// the program's stream, so it cannot split an escape sequence, a UTF-8
/// character or a line the program is drawing.
///
/// Notes are written to fd 2 and the session's bytes to fd 1; under `-v`
/// both land on the same terminal, with nothing in the stream separating
/// them. `-v` is what someone reaches for when something is already wrong,
/// so the corruption used to appear exactly where it was most confusing.
///
/// **Nothing here is timed.** The program stops halfway through a CSI and
/// stays there until this test lets it past a fifo, so the window the note
/// has to land in is held open by the test rather than by a sleep. That
/// the client saw the hint *while* it was open is established the same
/// way: the keystroke behind it comes back echoed by the remote pty, and
/// the client cannot have forwarded the key without having read the
/// netwatch descriptor first — it was readable earlier, and the frame loop
/// reads it before stdin in the same pass.
#[test]
fn a_verbose_note_lands_at_a_boundary_of_the_programs_output() {
    const ROUNDS: usize = 4;
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let gate = remote.root.path().join("sequence-gate");
    make_fifo(&gate);
    // Each round: a marker at a boundary, then a CSI opened and left open
    // (no final byte) until the fifo is released, then the byte that ends
    // it. After the last one the program is left at a boundary, so the
    // detach at the end has one to print its own note at.
    let prog = format!(
        "n=0; while [ $n -lt {ROUNDS} ]; do n=$((n+1)); printf '#%d#' \"$n\"; \
         printf '\\033[%d;0;0' \"$n\"; cat '{}' >/dev/null; printf 'm'; done; \
         printf 'DONE'; sleep 60",
        gate.display()
    );
    let mut env = vec![
        // The held note must be released by the boundary, not by the
        // bound behind it: the round trip below is not timed, and a loaded
        // machine may take a while over it.
        ("ACS_NOTE_HOLD_MS".to_string(), "600000".to_string()),
    ];
    env.extend(watch.env());
    let mut c = Client::start_env(
        &remote,
        &["-v", "devbox", "note", "--", "/bin/sh", "-c", &prog],
        &refs(&env),
    );
    // `-v` echoes the whole prelude, so the session announcement anchors
    // the short needles that follow (acs-ryz).
    c.wait_session("note");
    for round in 1..=ROUNDS {
        c.wait_for(&format!("#{round}#"), T);
        // On the terminal, and unfinished: the client's observer is inside
        // this CSI until the program ends it.
        c.wait_for(&format!("\x1b[{round};0;0"), T);
        // The kernel says something about the network. Under `-v` that is
        // a note, raised with the terminal in the middle of that sequence.
        watch.hint();
        // A keystroke behind it: the remote pty echoes it, so seeing it
        // back says the client has been round its loop since the hint —
        // and `<` is a CSI parameter byte, so the sequence is still open.
        c.send(b"<");
        c.wait_for("<", T);
        // Only now may the program finish the sequence. Nothing waits for
        // the note here: where it comes out is the assertion, and a wait
        // would only find it in the wrong place and then time out.
        release_fifo(&gate);
    }
    // The last note is written when the program's last sequence ends,
    // which is the frame `DONE` arrives in or an earlier one: by here,
    // every one of them has been printed.
    c.wait_for("DONE", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0, "{}", c.text());
    c.wait_for("detached from devbox/note", T);
    // Every hint said its line, once — held is delayed, not dropped, and
    // not repeated.
    let hint = "acs: network hint: still on 192.168.1.5/24 — not a change";
    assert_eq!(c.text().matches(hint).count(), ROUNDS, "{}", c.text());
    // And not one of them, nor the prelude's or the detach's, landed
    // inside the program's stream.
    assert!(notes_at_a_boundary(&c.output()) > ROUNDS);
}

/// acs-z22: what is held is delayed, never dropped. A program that stops
/// halfway through a sequence and says nothing more would otherwise keep
/// a note for ever — so the wait is bounded (`ACS_NOTE_HOLD_MS`), and the
/// note is printed where it stands rather than lost.
#[test]
fn a_note_is_not_swallowed_by_a_session_that_goes_quiet() {
    let remote = Remote::installed();
    let watch = NetWatch::new("192.168.1.5/24\n");
    let mut env = vec![("ACS_NOTE_HOLD_MS".to_string(), "300".to_string())];
    env.extend(watch.env());
    let mut c = Client::start_env(
        &remote,
        &[
            "-v",
            "devbox",
            "quiet",
            "--",
            "/bin/sh",
            "-c",
            "printf '#1#'; printf '\\033[1;0;0'; sleep 60",
        ],
        &refs(&env),
    );
    c.wait_session("quiet");
    c.wait_for("\x1b[1;0;0", T);
    // The sequence is never finished and no further frame ever comes. The
    // note arrives all the same.
    watch.hint();
    c.wait_for(
        "acs: network hint: still on 192.168.1.5/24 — not a change",
        T,
    );
}

/// acs-gov: `-v` names the whole ssh command line, and that line carries
/// the remote prelude — 1046 bytes since the symlink check, against 518
/// before it. `note`'s cap ended the line in the middle of the prelude,
/// taking the acs arguments at the end of it with it: the one part of that
/// line that says what this dial is actually for. The line may still be
/// truncated (a `…` says so), but not before those arguments.
#[test]
fn the_verbose_command_line_reaches_the_acs_arguments() {
    let remote = Remote::installed();
    // Through a fake `ssh` rather than `--transport-cmd`: the options a real
    // dial carries are a couple of hundred characters of the line, and the
    // cap bit only once they were in front of the prelude.
    let ssh = Ssh::new(&[("devbox", &remote)]);
    let mut c = Client::spawn(
        &exe(),
        &[
            "--ssh",
            &ssh.path().display().to_string(),
            "-p",
            "2222",
            "-v",
            "devbox",
            "vline",
            "--",
            "/bin/sh",
            "-c",
            "echo up; sleep 30",
        ],
        &[],
    );
    c.wait_for("acs: running ", T);
    // The prelude's own text comes first, then what acs was asked to run.
    c.wait_for("acs_safe", T);
    c.wait_for("_proxy", T);
    c.wait_for("--session vline", T);
    c.send(&command(b'x'));
    c.wait(T);
}
