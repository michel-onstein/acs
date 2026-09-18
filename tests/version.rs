//! `acs --version`.

mod common;

use std::os::fd::OwnedFd;
use std::process::Stdio;

use common::*;

#[test]
fn version_names_the_release_and_protocol() {
    let out = acs_cmd().arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(&format!("acs {} (protocol ", acs::VERSION)),
        "{text}"
    );
}

/// Regression: `acs --version | grep -q …` closes the pipe before acs has
/// written everything, and println! panicked on the EPIPE.
#[test]
fn version_into_a_closed_pipe_does_not_panic() {
    let (read, write): (OwnedFd, OwnedFd) = acs::sys::pipe().unwrap();
    drop(read);
    let out = acs_cmd()
        .arg("--version")
        .stdout(Stdio::from(write))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.status.success(), "{:?}: {err}", out.status);
}
