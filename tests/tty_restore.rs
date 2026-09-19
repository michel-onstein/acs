//! The client's terminal is restored after a fatal signal and after a panic
//! (DESIGN §7). Each case re-runs this test binary as a child on a pty, lets
//! it enter raw mode, kills or panics it, and inspects the pty afterwards.

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use acs::sys;

const CHILD_ENV: &str = "ACS_TTY_CHILD";

/// The child side. Does nothing unless re-run by a parent test.
#[test]
fn child_helper() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    acs::tty::install_emergency_restore().unwrap();
    let raw = acs::tty::RawMode::enter(0).unwrap();
    // Only the emergency paths may restore: never run the destructor.
    std::mem::forget(raw);
    if mode == "menu" {
        // The session menu's alternate screen.
        std::mem::forget(acs::tty::AltScreen::enter(1).unwrap());
    }
    sys::write_all(1, b"RAW\r\n").unwrap();
    match mode.as_str() {
        "signal" | "menu" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        "panic" => panic!("boom"),
        other => panic!("unknown mode {other}"),
    }
}

fn spawn_child(
    mode: &str,
) -> (
    std::process::Child,
    std::os::fd::OwnedFd,
    std::os::fd::OwnedFd,
) {
    let (master, slave) = sys::openpty().unwrap();
    let dup = |fd: &std::os::fd::OwnedFd| {
        let n = unsafe { libc::dup(fd.as_raw_fd()) };
        assert!(n >= 0);
        unsafe { Stdio::from_raw_fd(n) }
    };
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_helper", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, mode)
        .stdin(dup(&slave))
        .stdout(dup(&slave))
        .stderr(dup(&slave))
        .spawn()
        .unwrap();
    (child, master, slave)
}

fn wait_for_raw(master: &std::os::fd::OwnedFd) {
    let mut f = std::fs::File::from(master.try_clone().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    while !String::from_utf8_lossy(&seen).contains("RAW") {
        assert!(Instant::now() < deadline, "child never reported raw mode");
        let n = f.read(&mut buf).unwrap();
        seen.extend_from_slice(&buf[..n]);
    }
}

fn echo_on(slave: &std::os::fd::OwnedFd) -> bool {
    let t = sys::tcgetattr(slave.as_raw_fd()).unwrap();
    t.c_lflag & libc::ECHO != 0 && t.c_lflag & libc::ICANON != 0
}

#[test]
fn terminal_restored_after_fatal_signal() {
    let (mut child, master, slave) = spawn_child("signal");
    assert!(echo_on(&slave));
    wait_for_raw(&master);
    assert!(!echo_on(&slave), "child should be in raw mode");
    sys::kill(child.id() as i32, libc::SIGTERM).unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(echo_on(&slave), "terminal left in raw mode after SIGTERM");
}

#[test]
fn terminal_restored_after_panic() {
    let (mut child, master, slave) = spawn_child("panic");
    wait_for_raw(&master);
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(echo_on(&slave), "terminal left in raw mode after a panic");
}

#[test]
fn alternate_screen_left_after_fatal_signal() {
    let (mut child, master, slave) = spawn_child("menu");
    wait_for_raw(&master);
    sys::kill(child.id() as i32, libc::SIGTERM).unwrap();
    // What the child writes after RAW: the way back to the normal screen.
    // Read it before waiting: restoring the settings waits for the
    // terminal to take the output.
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.ends_with(b"\x1b[?25h\x1b[?1049l") {
        assert!(Instant::now() < deadline, "not left: {seen:?}");
        let mut p = [sys::pollfd(master.as_raw_fd(), libc::POLLIN)];
        if sys::poll(&mut p, 100).unwrap() > 0 {
            let n = sys::read(master.as_raw_fd(), &mut buf).unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
    }
    child.wait().unwrap();
    assert!(echo_on(&slave), "terminal left in raw mode after SIGTERM");
}
