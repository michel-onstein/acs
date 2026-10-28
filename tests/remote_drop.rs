//! What dropping a fake remote may and may not do while other tests run
//! (acs-hx0m).

mod common;

use common::*;

/// Regression (acs-hx0m): a remote's acs is a hard link of the binary every
/// test's client runs, and on macOS unlinking it while another test thread
/// exec'd that binary got the exec SIGKILLed — a client with no output and a
/// test timing out thirty seconds later. Dropping a remote must leave its
/// install in place for the exit handler to remove.
#[test]
fn dropping_a_remote_leaves_its_installed_binary_until_exit() {
    let r = Remote::installed();
    let acs = r.installed_binary(acs::VERSION);
    assert!(acs.exists());
    drop(r);
    assert!(acs.exists(), "a drop unlinked {}", acs.display());
}

/// The drop still ends the remote's sessions at once, as the directory's own
/// drop did: a master must not outlive its test.
#[test]
fn dropping_a_remote_ends_its_sessions() {
    let r = Remote::installed();
    let mut c = Client::start(
        &r,
        &["devbox", "kept", "--", "/bin/sh", "-c", "echo up; sleep 60"],
    );
    c.wait_for("up", T);
    drop(c);
    let sock = r.sockets().join("kept.sock");
    let pid = acs::testutil::session_pid(&sock).expect("session running");
    drop(r);
    let deadline = std::time::Instant::now() + T;
    while acs::sys::kill(pid as i32, 0).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "master {pid} outlived its remote"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
