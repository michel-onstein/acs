//! `cargo xtask package` turns a dist directory into release assets.

use std::path::PathBuf;
use std::process::Command;

/// `channel.stamp` as `cargo xtask dist` writes it.
fn stamp(releases: &str, key: &str) -> String {
    format!("releases {releases}\nkey {key}\n")
}

#[test]
fn packages_every_target_with_checksums_and_notes() {
    let root = std::env::temp_dir().join(format!("acs-package-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    let out = root.join("assets");
    for t in ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"] {
        std::fs::create_dir_all(dist.join(t)).unwrap();
        std::fs::write(dist.join(t).join("acs"), format!("binary for {t}")).unwrap();
    }
    // Not a target: no `acs` inside.
    std::fs::write(dist.join("payloads.bin"), b"blob").unwrap();
    // Built for this xtask's channel, as `cargo xtask dist` records it.
    std::fs::write(
        dist.join("channel.stamp"),
        stamp(
            acs::release::DEFAULT_RELEASES_URL,
            acs::signature::RELEASE_KEY,
        ),
    )
    .unwrap();
    std::fs::write(root.join("changes.md"), "- fix: something\n").unwrap();
    let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../README.md");

    let st = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["package", "--version", "v0.9.1", "--dist"])
        .arg(&dist)
        .arg("--out")
        .arg(&out)
        .arg("--readme")
        .arg(&readme)
        .arg("--installer")
        .arg(readme.parent().unwrap().join("scripts/install.sh"))
        .arg("--changes")
        .arg(root.join("changes.md"))
        .status()
        .unwrap();
    assert!(st.success());

    let mut files: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "NOTES.md",
            "SHA256SUMS",
            "acs-0.9.1-aarch64-apple-darwin.tar.gz",
            "acs-0.9.1-x86_64-unknown-linux-musl.tar.gz",
            "install.sh"
        ]
    );

    // The archive holds the binary (executable) and the README in a folder.
    let list = Command::new("tar")
        .arg("-tvzf")
        .arg(out.join("acs-0.9.1-x86_64-unknown-linux-musl.tar.gz"))
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    let acs_line = list
        .lines()
        .find(|l| l.ends_with("acs-0.9.1-x86_64-unknown-linux-musl/acs"))
        .unwrap_or_else(|| panic!("{list}"));
    assert!(acs_line.starts_with("-rwxr-xr-x"), "{acs_line}");
    assert!(
        list.contains("acs-0.9.1-x86_64-unknown-linux-musl/README.md"),
        "{list}"
    );

    // The checksums verify with the standard tool.
    let check = Command::new("shasum")
        .args(["-a", "256", "-c", "SHA256SUMS"])
        .current_dir(&out)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );

    // The installer ships executable, and carrying this build's releases
    // URL and release key rather than whatever the checked-in script says
    // (acs-x57) — here through the real command, not the function.
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(out.join("install.sh")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    let installer = std::fs::read_to_string(out.join("install.sh")).unwrap();
    assert!(installer.starts_with("#!/bin/sh\n"));
    assert!(
        installer.contains(&format!(
            "default_releases='{}'\n",
            acs::release::DEFAULT_RELEASES_URL
        )),
        "the packaged installer does not carry this build's releases"
    );
    assert!(
        installer.contains(&format!("release_key='{}'\n", acs::signature::RELEASE_KEY)),
        "the packaged installer does not carry this build's release key"
    );
    // And it is still a script `sh` will read.
    let syntax = Command::new("sh")
        .arg("-n")
        .arg(out.join("install.sh"))
        .status()
        .unwrap();
    assert!(syntax.success(), "the packaged installer does not parse");

    let notes = std::fs::read_to_string(out.join("NOTES.md")).unwrap();
    assert!(notes.contains("acs 0.9.1"));
    assert!(notes.contains("- fix: something"));
    assert!(
        notes.contains(&format!(
            "curl -fsSL {}/latest/download/install.sh | sh",
            acs::release::DEFAULT_RELEASES_URL
        )),
        "{notes}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// acs-u3qn: `dist` run with a fork's `ACS_DEFAULT_RELEASES_URL` and
/// `package` run without it used to put the fork's binaries beside acs's
/// installer, notes and formula. A dist/ built for another channel — or one
/// that does not say — is now refused, and nothing is written.
#[test]
fn a_dist_for_another_channel_is_not_packaged() {
    let root = std::env::temp_dir().join(format!("acs-package-channel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    let t = "x86_64-unknown-linux-musl";
    std::fs::create_dir_all(dist.join(t)).unwrap();
    std::fs::write(dist.join(t).join("acs"), "a fork's binary").unwrap();
    let fork_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork";
    let package = |out: &str| {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["package", "--version", "0.9.1", "--dist"])
            .arg(&dist)
            .arg("--out")
            .arg(root.join(out))
            .output()
            .unwrap()
    };

    for (case, text) in [
        (
            "releases",
            stamp("https://dl.example.com/acs", acs::signature::RELEASE_KEY),
        ),
        ("key", stamp(acs::release::DEFAULT_RELEASES_URL, fork_key)),
    ] {
        std::fs::write(dist.join("channel.stamp"), text).unwrap();
        let out = package(case);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{case}: packaged a mismatched pair");
        assert!(
            err.contains("built for another release channel"),
            "{case}: {err}"
        );
        assert!(!root.join(case).exists(), "{case}: wrote assets anyway");
    }

    std::fs::remove_file(dist.join("channel.stamp")).unwrap();
    let out = package("none");
    assert!(!out.status.success(), "packaged an unstamped dist/");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("channel.stamp"),
        "{out:?}"
    );
    assert!(!root.join("none").exists());

    let _ = std::fs::remove_dir_all(&root);
}

/// acs-u3qn: a mistyped `--targets` is refused before anything is built or
/// the output directory is wiped, rather than leaving a dist/ a platform
/// short.
#[test]
fn dist_refuses_a_target_acs_does_not_release() {
    let root = std::env::temp_dir().join(format!("acs-dist-targets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("keep"), "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "dist",
            "--targets",
            "x86_64-unknown-linux-musl,aarch64-apple-darwn",
            "--out",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("\"aarch64-apple-darwn\" is not a target"),
        "{err}"
    );
    assert!(!err.contains("+ "), "it started a build: {err}");
    assert!(root.join("keep").exists(), "the output directory was wiped");
    let _ = std::fs::remove_dir_all(&root);
}

/// acs-u3qn: an xtask compiled for one channel but run in an environment
/// that builds for another — a cached binary — would stamp the binaries
/// with the wrong channel, so `dist` refuses before it builds anything.
#[test]
fn dist_refuses_an_environment_for_another_channel() {
    let root = std::env::temp_dir().join(format!("acs-dist-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("keep"), "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["dist", "--targets", "x86_64-unknown-linux-musl", "--out"])
        .arg(&root)
        .env("ACS_DEFAULT_RELEASES_URL", "https://elsewhere.example/acs")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("https://elsewhere.example/acs"), "{err}");
    assert!(!err.contains("+ "), "it started a build: {err}");
    assert!(root.join("keep").exists(), "the output directory was wiped");
    let _ = std::fs::remove_dir_all(&root);
}
