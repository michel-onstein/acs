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
    assert!(
        lines[2].ends_with("master.log"),
        "log_master lost: {text:?}"
    );
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
