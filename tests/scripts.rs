//! The container scripts keep each checkout apart: worktrees run them at
//! once, and a shared target volume let `scripts/test_linux.sh` pass on
//! another worktree's build. Run against fake `docker`, `ssh`, `ssh-keygen`
//! and `cargo` that only record how they were called.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use acs::testutil::TempDir;

const FAKE: &str = r#"#!/bin/sh
# Records one line per call: the program's name and its arguments.
echo "$(basename "$0") $*" >>"$FAKE_LOG"
case "$(basename "$0") $1" in
    "docker port") echo 127.0.0.1:49999 ;;
    "ssh-keygen "*) while [ $# -gt 1 ]; do shift; done; : >"$1"; : >"$1.pub" ;;
    "cargo "*) echo "cargo-env port=$ACS_E2E_PORT container=$ACS_E2E_CONTAINER" >>"$FAKE_LOG" ;;
esac
"#;

/// A copy of the repository's scripts at `<dir>/<name>`, as a checkout.
fn checkout(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    let scripts = root.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for s in ["test_linux.sh", "e2e_ssh.sh"] {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join(s);
        std::fs::copy(src, scripts.join(s)).unwrap();
    }
    root
}

/// Runs `script` of `root` with the fakes first on PATH; returns the log.
fn run(dir: &Path, root: &Path, script: &str, args: &[&str]) -> Vec<String> {
    let bin = dir.join("bin");
    if !bin.exists() {
        std::fs::create_dir(&bin).unwrap();
        for p in ["docker", "ssh", "ssh-keygen", "cargo"] {
            let f = bin.join(p);
            std::fs::write(&f, FAKE).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let log = dir.join("log");
    let _ = std::fs::remove_file(&log);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = Command::new("sh")
        .arg(root.join("scripts").join(script))
        .args(args)
        .env("PATH", path)
        .env("FAKE_LOG", &log)
        .env_remove("ACS_E2E_PORT")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// The word after `flag` in the first logged line starting with `call`.
fn arg_after(log: &[String], call: &str, flag: &str) -> String {
    let line = log.iter().find(|l| l.starts_with(call)).expect(call);
    let words: Vec<&str> = line.split(' ').collect();
    let i = words.iter().position(|w| *w == flag).expect(flag);
    words[i + 1].to_string()
}

fn target_volume(log: &[String]) -> String {
    let line = log.iter().find(|l| l.starts_with("docker run")).unwrap();
    line.split(' ')
        .find(|w| w.starts_with("acs-linux-target"))
        .expect("a target volume")
        .to_string()
}

#[test]
fn test_linux_gives_each_checkout_its_own_target_volume() {
    let dir = TempDir::new();
    let (a, b) = (checkout(dir.path(), "a"), checkout(dir.path(), "b"));
    let va = target_volume(&run(dir.path(), &a, "test_linux.sh", &[]));
    let vb = target_volume(&run(dir.path(), &b, "test_linux.sh", &[]));
    assert_ne!(va, vb, "two checkouts shared {va}");
    // Stable, so a checkout's next run reuses its build.
    assert_eq!(
        va,
        target_volume(&run(dir.path(), &a, "test_linux.sh", &[]))
    );
}

#[test]
fn e2e_gives_each_checkout_its_own_host_on_a_free_port() {
    let dir = TempDir::new();
    let (a, b) = (checkout(dir.path(), "a"), checkout(dir.path(), "b"));
    let la = run(dir.path(), &a, "e2e_ssh.sh", &["--no-build"]);
    let lb = run(dir.path(), &b, "e2e_ssh.sh", &["--no-build"]);
    let (na, nb) = (
        arg_after(&la, "docker run", "--name"),
        arg_after(&lb, "docker run", "--name"),
    );
    assert_ne!(na, nb, "two checkouts shared the container {na}");
    // No fixed host port: docker picks one, and the tests are told which.
    assert_eq!(arg_after(&la, "docker run", "-p"), "127.0.0.1::22");
    assert_eq!(arg_after(&la, "ssh ", "-p"), "49999");
    assert!(
        la.contains(&format!("cargo-env port=49999 container={na}")),
        "{la:?}"
    );
    // Removing a leftover host touches only this checkout's.
    for l in la.iter().filter(|l| l.starts_with("docker rm")) {
        assert!(l.ends_with(&na), "{l}");
    }
}
