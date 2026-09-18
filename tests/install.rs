//! Remote self-install (acs-5v9.21, DESIGN §8, §8.1).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use common::*;

const CMD: &[&str] = &["--", "/bin/sh", "-c", "echo up; sleep 30"];

fn args(session: &str) -> Vec<&str> {
    let mut a = vec!["devbox", session];
    a.extend_from_slice(CMD);
    a
}

fn leftovers(remote: &Remote) -> Vec<String> {
    let dir = remote.installed_binary(acs::VERSION);
    std::fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n != "acs")
        .collect()
}

#[test]
fn same_platform_installs_a_copy_of_itself() {
    let remote = Remote::new();
    let mut c = Client::start(&remote, &args("s"));
    c.wait_for(&format!("installing acs {} on devbox (", acs::VERSION), T);
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
    let installed = remote.installed_binary(acs::VERSION);
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(exe()).unwrap()
    );
    assert_eq!(
        std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let link = remote.home().join(".local/bin/acs");
    assert_eq!(std::fs::read_link(&link).unwrap(), installed);
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

#[test]
fn a_slim_build_cannot_install_another_platform() {
    let remote = Remote::new();
    remote.fake_uname("Linux", "x86_64");
    let mut c = Client::start(&remote, &args("s"));
    assert_eq!(c.wait(T), 254);
    c.wait_for("cargo xtask dist", T);
    assert!(!remote.installed_binary(acs::VERSION).exists());
}

#[test]
fn a_complete_build_installs_another_platform_from_its_payloads() {
    let remote = Remote::new();
    // Pretend the host is Linux x86_64; the payload for it is this very
    // binary, so what gets installed can run here.
    remote.fake_uname("Linux", "x86_64");
    let (client, slim, blob) = complete_client(remote.root.path(), "x86_64-unknown-linux-musl");
    let mut c = Client::start_exe(&client, &remote, &args("p"), &[]);
    c.wait_for("(Linux x86_64)", T);
    c.wait_for("up", T);
    // The installed copy is slim + payloads: byte-identical to the complete
    // binary for that target, so it can install onward.
    let mut expected = slim;
    expected.extend_from_slice(&blob);
    let installed = std::fs::read(remote.installed_binary(acs::VERSION)).unwrap();
    assert!(
        installed == expected,
        "installed binary differs from the complete build"
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

#[test]
fn concurrent_installs_converge() {
    let remote = Remote::new();
    let mut a = Client::start(&remote, &args("a"));
    let mut b = Client::start(&remote, &args("b"));
    a.wait_for("up", T);
    b.wait_for("up", T);
    assert_eq!(
        std::fs::read(remote.installed_binary(acs::VERSION)).unwrap(),
        std::fs::read(exe()).unwrap()
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

#[test]
fn other_versions_are_left_alone() {
    let remote = Remote::new();
    let other = remote.home().join(".local/share/acs/0.0.1-old/acs");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, b"#!/bin/sh\necho old\n").unwrap();
    let mut c = Client::start(&remote, &args("v"));
    c.wait_for("up", T);
    assert_eq!(std::fs::read(&other).unwrap(), b"#!/bin/sh\necho old\n");
    assert!(remote.installed_binary(acs::VERSION).exists());
}

#[test]
fn finisher_rejects_a_corrupt_upload() {
    let remote = Remote::new();
    let dir = remote
        .home()
        .join(format!(".local/share/acs/{}", acs::VERSION));
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join("acs.new.00ff");
    std::fs::copy(exe(), &tmp).unwrap();
    let out = Command::new(&tmp)
        .args([
            "_install", "--finish", "--token", "00ff", "--sha256", "0000",
        ])
        .env("HOME", remote.home())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert!(!tmp.exists(), "corrupt upload must be removed");
    assert!(!dir.join("acs").exists());
}
