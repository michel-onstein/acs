//! Smoke test of the integration harness (acs-5v9.14): the full remote chain
//! — prelude, versioned binary, proxy, master, pty — carries a program's
//! output byte for byte, without ssh.

mod common;

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
