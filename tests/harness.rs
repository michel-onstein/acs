//! Smoke test of the integration harness (acs-5v9.14): the full remote chain
//! — prelude, versioned binary, proxy, master, pty — carries a program's
//! output byte for byte, without ssh.

mod common;

use std::process::Command;

use acs::proto::{Marker, Mode, Msg};
use acs::testutil::{hello, FrameConn};
use common::*;

#[test]
fn remote_chain_is_byte_exact() {
    let remote = Remote::installed();
    let data_file = remote.root.path().join("data");
    // Raw bytes: control characters, escape sequences, high bytes.
    let data = binary_pattern(&data_file, 200_000);

    let mut child =
        remote.run_remote(&["_proxy", "--session", "smoke", "--mode", "attach-or-create"]);
    let mut c = FrameConn::from_io(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    assert!(matches!(c.expect_marker(T), Some(Marker::Ready { .. })));
    let mut h = hello("smoke", Mode::AttachOrCreate, "me");
    // Raw mode on the pty so the terminal line discipline does not touch
    // the bytes, then print them and a marker.
    h.command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "stty raw -echo; cat '{}'; printf '\\nEND'",
            data_file.display()
        ),
    ];
    c.send(&Msg::Hello(h));
    match c.recv_control(T) {
        Some(Msg::Welcome(w)) => assert!(w.created),
        other => panic!("{other:?}"),
    }
    c.wait_output("\nEND", T);
    let out = &c.output;
    let start = out
        .windows(data.len().min(64))
        .position(|w| w == &data[..64])
        .expect("pattern start in output");
    assert_eq!(
        &out[start..start + data.len()],
        &data[..],
        "bytes changed in transit"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn missing_binary_reports_need() {
    let remote = Remote::new();
    let mut child = remote.run_remote(&["_proxy", "--list"]);
    let mut c = FrameConn::from_io(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    match c.expect_marker(T) {
        Some(Marker::Need { os, arch }) => {
            assert!(!os.is_empty() && !arch.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let _ = child.wait();
}

#[test]
fn dropping_a_remote_ends_its_sessions() {
    // Regression: test sessions used to outlive their tests forever.
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &[
            "devbox",
            "keep",
            "--",
            "/bin/sh",
            "-c",
            "trap '' HUP; echo spinning; while :; do sleep 0.1; done",
        ],
    );
    c.wait_for("spinning", T);
    let pid = acs::testutil::session_pid(&remote.sockets().join("keep.sock")).unwrap();
    drop(c);
    drop(remote);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while acs::sys::kill(pid as i32, 0).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "master {pid} still running"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Regression (acs-fzx): the helpers that configure the remote side all
/// write one file, and `remote_env` used to write it *whole* — so calling
/// it after `remote_umask` or `log_master` silently discarded their
/// settings, leaving a test that still passed while no longer configuring
/// what its name said it configured. Set three of them in that once-fatal
/// order and ask the transport what the remote side actually sees: every
/// setting must be there.
#[test]
fn remote_env_helpers_compose_in_any_order() {
    let remote = Remote::new();
    remote.remote_umask("077");
    remote.log_master();
    remote.remote_env(&[("ACS_DEAD_MS", "60000")]);

    let out = output_of(
        Command::new(remote.transport())
            .arg("umask; printf '%s\\n' \"$ACS_DEAD_MS\" \"$ACS_MASTER_LOG\""),
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert_eq!(lines.len(), 3, "{text:?}");
    // Printed as `077` or `0077` depending on the shell; both are 0o77.
    assert_eq!(
        u32::from_str_radix(lines[0], 8).ok(),
        Some(0o77),
        "umask lost: {text:?}"
    );
    assert_eq!(lines[1], "60000", "remote_env lost: {text:?}");
    // The exact path, not merely one ending in `master.log` (acs-9jv): the
    // weaker assertion passes with a path bug sitting under it, and the
    // path is the one thing `log_master` exists to get right.
    assert_eq!(
        lines[2],
        remote.master_log_file().display().to_string(),
        "log_master lost: {text:?}"
    );
}

/// acs-9jv: `transport()` writes the script from the remote's current state
/// on every call instead of trusting whatever is already at the path, so a
/// setting that one day changes the script *body* cannot silently no-op
/// because some earlier call had already materialised the file.
#[test]
fn the_transport_script_is_rewritten_rather_than_trusted() {
    let remote = Remote::new();
    let path = std::path::PathBuf::from(remote.transport());
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains(&remote.home().display().to_string()),
        "{body}"
    );

    std::fs::write(&path, "#!/bin/sh\nexit 7\n").unwrap();
    assert_eq!(remote.transport(), path.display().to_string());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    // And still executable: the rewrite goes through a fresh file.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "{mode:o}");
}

/// acs-9jv: the one splitter looks at ssh's options only. The remote
/// prelude after the `--` is full of text shaped exactly like an option —
/// `ls -ldnL`, `[ -L … ]`, a trailing `-i` — so a test that searches the
/// whole recorded line finds a forward in a call that carries none, and
/// passes while asserting something untrue (acs-odd hit this on the
/// `Call::Session` forwarding gate).
#[test]
fn splitting_a_recorded_call_ignores_the_prelude() {
    let call = "-T -e none -o BatchMode=yes -i /keys/id -p 2222 -- me@devbox.lan \
                p=$HOME/.local/share/acs/1.2.3/acs; [ -L \"$p\" ] && exit 1; \
                ls -ldnL \"$p\"; exec \"$p\" _proxy -i";
    let c = SshCall::of(call);
    assert_eq!(c.dest, "me@devbox.lan");
    assert_eq!(c.opts, "-T -e none -o BatchMode=yes -i /keys/id -p 2222");
    assert!(c.remote.starts_with("p=$HOME/"), "{:?}", c.remote);
    // The prelude's `-L`s and `-i` are not options.
    assert!(!c.opts.contains("-L"), "{:?}", c.opts);
    assert!(c.opt_values("-L").is_empty(), "{:?}", c.opts);
    assert_eq!(c.opt_values("-i"), ["/keys/id"]);

    // A call that really does carry forwards, in both of ssh's spellings.
    let f = SshCall::of("-T -L 45997:localhost:9 -L45998:db:5432 -- devbox ls -ldnL x");
    assert_eq!(f.dest, "devbox");
    assert_eq!(f.opt_values("-L"), ["45997:localhost:9", "45998:db:5432"]);

    // A call with no remote command at all.
    let bare = SshCall::of("-T -- devbox");
    assert_eq!((bare.opts, bare.dest, bare.remote), ("-T", "devbox", ""));
}

/// Regression: waiting twice for the same text waits for its second
/// occurrence — the search backed up over the first match and found it
/// again, so the second wait returned at once.
#[test]
fn waiting_twice_for_the_same_text_needs_it_twice() {
    let remote = Remote::installed();
    let mut c = Client::start(
        &remote,
        &[
            "devbox",
            "twice",
            "--",
            "/bin/sh",
            "-c",
            "echo tick; sleep 1; echo tick; sleep 30",
        ],
    );
    c.wait_for("tick", T);
    let t0 = std::time::Instant::now();
    c.wait_for("tick", T);
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
}
