//! `acs upgrade` against a fake release server (acs-uko, DESIGN §7.5). The
//! real curl downloads; the "releases" hold scripts that answer --version.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use acs::testutil::TempDir;
use common::*;

const NEW: &str = "9.9.9";

/// A copy of this acs as a plain file in `dir`.
fn plain_copy(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let p = bin.join("acs");
    std::fs::copy(exe(), &p).unwrap();
    p
}

/// Run `acs upgrade <args>` as `path`, with releases from `server`.
fn upgrade(
    path: &Path,
    home: &Path,
    server: &ReleaseServer,
    args: &[&str],
) -> (i32, String, String) {
    let out = output_of(
        Command::new(path)
            .arg("upgrade")
            .args(args)
            .env("ACS_RELEASES_URL", &server.url)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
    );
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn version_of(path: &Path) -> String {
    let out = output_of(Command::new(path).arg("--version"));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

#[test]
fn upgrades_a_plain_binary_in_place() {
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let acs = plain_copy(d.path());
    let (code, out, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "upgraded acs {} → {NEW} ({})\n",
            acs::VERSION,
            std::fs::canonicalize(&acs).unwrap().display()
        )
    );
    assert_eq!(std::fs::read(&acs).unwrap(), ReleaseServer::fake_acs(NEW));
    assert_eq!(
        std::fs::metadata(&acs).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(version_of(&acs).starts_with(&format!("acs {NEW} ")));
    // Nothing is left next to it.
    let names: Vec<_> = std::fs::read_dir(acs.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, ["acs"]);
}

#[test]
fn a_versioned_install_gets_the_new_version_beside_it() {
    let d = TempDir::new();
    let home = d.path().join("home");
    let share = home.join(".local/share/acs");
    let old = share.join(acs::VERSION).join("acs");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::copy(exe(), &old).unwrap();
    let link = home.join(".local/bin/acs");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&old, &link).unwrap();

    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let (code, out, err) = upgrade(&link, &home, &server, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("→ {NEW} ({})", link.display())),
        "{out}"
    );
    let new = share.join(NEW).join("acs");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::fs::canonicalize(&new).unwrap()
    );
    assert!(version_of(&link).starts_with(&format!("acs {NEW} ")));
    // The old version stays for clients that still use it on this host.
    assert_eq!(std::fs::read(&old).unwrap(), std::fs::read(exe()).unwrap());
}

#[test]
fn a_checksum_mismatch_changes_nothing() {
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, true);
    let acs = plain_copy(d.path());
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 1);
    assert!(err.contains("checksum mismatch for acs-9.9.9-"), "{err}");
    assert!(err.contains("nothing was changed"), "{err}");
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
}

#[test]
fn already_current_or_newer_does_nothing() {
    let d = TempDir::new();
    let acs = plain_copy(d.path());

    let server = ReleaseServer::start();
    server.release(acs::VERSION, b"not used", true, false);
    let (code, out, _) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!(
            "acs {} is the latest release; nothing to do\n",
            acs::VERSION
        )
    );
    assert!(!server.hits().iter().any(|h| h.ends_with(".tar.gz")));

    let server = ReleaseServer::start();
    server.release("0.0.1", &ReleaseServer::fake_acs("0.0.1"), true, false);
    let (code, out, _) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 0);
    assert!(
        out.contains("is newer than the latest release (0.0.1)"),
        "{out}"
    );
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());

    // Asking for that version by name downgrades.
    let (code, out, err) = upgrade(&acs, d.path(), &server, &["--version", "0.0.1"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("→ 0.0.1"), "{out}");
    assert!(version_of(&acs).starts_with("acs 0.0.1 "));
}

#[test]
fn check_only_reports() {
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let acs = plain_copy(d.path());
    let (code, out, _) = upgrade(&acs, d.path(), &server, &["--check"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!(
            "acs {NEW} is available (you have {}) — run: acs upgrade\n",
            acs::VERSION
        )
    );
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
    assert_eq!(server.hits(), ["/latest/download/SHA256SUMS"]);
}

#[test]
fn a_brewed_acs_defers_to_brew() {
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let acs = brewed_copy(d.path());
    let keg = std::fs::canonicalize(&acs).unwrap();
    let (code, out, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        err,
        format!(
            "acs: this acs was installed with Homebrew ({}) — upgrade it with: brew upgrade acs\n",
            keg.display()
        )
    );
    // Refused before asking: nothing downloaded, nothing replaced.
    assert!(server.hits().is_empty(), "{:?}", server.hits());
    assert_eq!(std::fs::read(&keg).unwrap(), std::fs::read(exe()).unwrap());
    assert!(!d.path().join("homebrew/Cellar/acs").join(NEW).exists());

    // --check still reports, pointing at brew.
    let (code, out, _) = upgrade(&acs, d.path(), &server, &["--check"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!(
            "acs {NEW} is available (you have {}) — run: brew upgrade acs\n",
            acs::VERSION
        )
    );
}

#[test]
fn an_unwritable_directory_says_to_use_sudo_before_downloading() {
    if acs::sys::getuid() == 0 {
        return; // root writes anywhere
    }
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let acs = plain_copy(d.path());
    let dir = acs.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, 1);
    assert!(err.contains("re-run with sudo: sudo acs upgrade"), "{err}");
    assert_eq!(server.hits(), ["/latest/download/SHA256SUMS"]);
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
}

#[test]
fn a_download_that_is_not_what_it_says_is_refused() {
    let d = TempDir::new();
    let server = ReleaseServer::start();
    // Correct checksum, but the binary claims another version.
    server.release(NEW, &ReleaseServer::fake_acs("1.0.0"), true, false);
    let acs = plain_copy(d.path());
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 1);
    assert!(err.contains("does not report version 9.9.9"), "{err}");
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
}

/// Regression (acs-x1k): the "it runs here" check runs the new binary from
/// the directory it will live in, not from the scratch directory under
/// /tmp, which is mounted noexec on hardened hosts.
#[test]
fn the_new_binary_is_run_from_its_destination_directory() {
    let d = TempDir::new();
    let log = d.path().join("ran-from");
    let server = ReleaseServer::start();
    let fake = format!(
        "#!/bin/sh\necho 'acs {NEW} (protocol 9, {t})'\ndirname \"$0\" >> '{log}'\n",
        t = acs::payload::OWN_TARGET,
        log = log.display()
    );
    server.release(NEW, fake.as_bytes(), true, false);
    let acs = plain_copy(d.path());
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 0, "{err}");
    let first = std::fs::read_to_string(&log).unwrap();
    let first = first.lines().next().expect("the check ran the binary");
    assert_eq!(
        std::fs::canonicalize(first).unwrap(),
        std::fs::canonicalize(acs.parent().unwrap()).unwrap(),
        "checked from {first}, not from the destination directory"
    );
}

#[test]
fn a_missing_release_or_no_network_is_explained() {
    let d = TempDir::new();
    let acs = plain_copy(d.path());
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let (code, _, err) = upgrade(&acs, d.path(), &server, &["--version", "5.5.5"]);
    assert_eq!(code, 1);
    assert!(err.contains("(is 5.5.5 a release?)"), "{err}");

    let empty = ReleaseServer::start();
    let (code, _, err) = upgrade(&acs, d.path(), &empty, &[]);
    assert_eq!(code, 1);
    assert!(err.contains("cannot download"), "{err}");

    let (code, _, err) = upgrade(&acs, d.path(), &server, &["--version", "latest"]);
    assert_eq!(code, 2);
    assert!(err.contains("X.Y.Z"), "{err}");
}
