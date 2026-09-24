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

/// Regression (acs-28b): the payload path wrote the binary with the mode
/// the umask left it, so a host with `umask 077` got a 0700 binary while
/// the self-copy path chmods 0755. Both are 0755 now.
#[test]
fn an_install_is_0755_under_a_restrictive_umask() {
    for payloads in [false, true] {
        let remote = Remote::new();
        remote.remote_umask("077");
        let mut c = if payloads {
            remote.fake_uname("Linux", "x86_64");
            let (client, _, _) = complete_client(remote.root.path(), "x86_64-unknown-linux-musl");
            Client::start_exe(&client, &remote, &args("u"), &[])
        } else {
            Client::start(&remote, &args("u"))
        };
        c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
        c.wait_for("up", T);
        assert_eq!(
            std::fs::metadata(remote.installed_binary(acs::VERSION))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "payload path: {payloads}"
        );
    }
}

/// Regression (acs-iws): the install made its directory with a bare
/// `mkdir -p`, so `umask 002` left it 0775 — and the prelude's acs-08m check
/// then refused to exec the binary acs had just installed there ("refusing
/// to run …: it or its directory is writable by others", exit 254). The
/// install succeeded and every connection after it failed.
///
/// Both directories acs makes are 0755 now, whatever the umask, and the
/// session comes up.
#[test]
fn an_install_under_a_group_writable_umask_still_runs() {
    for payloads in [false, true] {
        let remote = Remote::new();
        remote.remote_umask("002");
        let mut c = if payloads {
            remote.fake_uname("Linux", "x86_64");
            let (client, _, _) = complete_client(remote.root.path(), "x86_64-unknown-linux-musl");
            Client::start_exe(&client, &remote, &args("u"), &[])
        } else {
            Client::start(&remote, &args("u"))
        };
        c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
        // The session comes up: nothing refused the freshly installed binary.
        c.wait_for("up", T);
        let binary = remote.installed_binary(acs::VERSION);
        let version_dir = binary.parent().unwrap();
        let acs_dir = version_dir.parent().unwrap();
        for d in [acs_dir, version_dir] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "{} (payload path: {payloads})", d.display());
        }
        assert_eq!(
            std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777,
            0o755,
            "payload path: {payloads}"
        );
    }
}

/// Regression (acs-q9t): login-shell noise on stdout must not fail the
/// payload upload step, which prints nothing of its own.
#[test]
fn a_chatty_login_shell_does_not_break_either_install() {
    let remote = Remote::new();
    remote.login_noise("Welcome to devbox!\nYou have mail.\n");
    remote.fake_uname("Linux", "x86_64");
    let (client, _, _) = complete_client(remote.root.path(), "x86_64-unknown-linux-musl");
    let mut c = Client::start_exe(&client, &remote, &args("p"), &[]);
    c.wait_for("(Linux x86_64)", T);
    c.wait_for("up", T);
    assert!(!c.text().contains("unexpected reply"), "{}", c.text());
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));

    let remote = Remote::new();
    remote.login_noise("motd\n");
    let mut c = Client::start(&remote, &args("s"));
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
}

/// Regression (acs-vdl): a login banner that is not UTF-8 must not hide the
/// finisher's `ok` line — reading the reply as a String threw it all away.
#[test]
fn a_login_banner_that_is_not_utf8_does_not_fail_the_install() {
    let remote = Remote::new();
    remote.login_noise_bytes(b"Welcome to d\xe9vbox \xff\n");
    let mut c = Client::start(&remote, &args("l"));
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
    assert!(!c.text().contains("unexpected reply"), "{}", c.text());
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

/// Regression (acs-iry): the link into ~/.local/bin is a convenience — when
/// it cannot be made (here ~/.local/bin is a regular file) the install still
/// counts, with a warning, and the session runs.
#[test]
fn a_link_that_cannot_be_made_is_a_warning_not_a_failed_install() {
    let remote = Remote::new();
    let local = remote.home().join(".local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("bin"), "not a directory\n").unwrap();
    let mut c = Client::start(&remote, &args("w"));
    c.wait_for("warning: cannot link ~/.local/bin/acs", T);
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
    assert!(remote.installed_binary(acs::VERSION).exists());
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

/// Regression (acs-14k): a startup file that prints without a trailing
/// newline must not hide the ACS-NEED or ACS-READY marker.
#[test]
fn noise_without_a_newline_does_not_hide_the_markers() {
    let remote = Remote::new();
    remote.login_noise("printf without newline");
    // ACS-NEED, then the install, then ACS-READY.
    let mut c = Client::start(&remote, &args("n"));
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
}

/// Regression (acs-tb1): installing does not replace a hand-installed
/// `~/.local/bin/acs`, nor move a link to a newer version backwards.
#[test]
fn the_bin_link_is_left_alone_unless_it_moves_forward() {
    let remote = Remote::new();
    let link = remote.home().join(".local/bin/acs");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::fs::write(&link, "#!/bin/sh\necho mine\n").unwrap();
    let mut c = Client::start(&remote, &args("f"));
    c.wait_for("up", T);
    assert!(remote.installed_binary(acs::VERSION).exists());
    assert_eq!(
        std::fs::read_to_string(&link).unwrap(),
        "#!/bin/sh\necho mine\n"
    );

    let remote = Remote::new();
    let newer = remote.home().join(".local/share/acs/99.0.0/acs");
    std::fs::create_dir_all(newer.parent().unwrap()).unwrap();
    std::fs::write(&newer, "newer").unwrap();
    let link = remote.home().join(".local/bin/acs");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&newer, &link).unwrap();
    let mut c = Client::start(&remote, &args("g"));
    c.wait_for("up", T);
    assert!(remote.installed_binary(acs::VERSION).exists());
    assert_eq!(std::fs::read_link(&link).unwrap(), newer);
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
    let out = output_of(
        Command::new(&tmp)
            .args([
                "_install", "--finish", "--token", "00ff", "--sha256", "0000",
            ])
            .env("HOME", remote.home()),
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert!(!tmp.exists(), "corrupt upload must be removed");
    assert!(!dir.join("acs").exists());
}

#[test]
fn old_unused_versions_are_pruned_at_proxy_start() {
    let remote = Remote::new();
    remote.install(acs::VERSION);
    let root = remote.home().join(".local/share/acs");
    let old = root.join("0.0.1-old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("acs"), b"old").unwrap();
    let ninety_days_ago = acs::sys::unix_now() - 90 * 86_400;
    acs::sys::set_mtime(&old, ninety_days_ago).unwrap();
    acs::sys::set_mtime(&root.join(acs::VERSION), ninety_days_ago).unwrap();

    let mut c = Client::start_env(&remote, &args("pr"), &[("ACS_PRUNE_EVERY_SECS", "0")]);
    c.wait_for("up", T);
    assert!(!old.exists(), "an old unused version must be pruned");
    // Our own version was marked as used just now.
    let age = std::fs::metadata(root.join(acs::VERSION))
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap();
    assert!(age.as_secs() < 3600, "{age:?}");
    assert!(root.join(".pruned").exists());
}

/// acs-4km: the shell checks the upload before it is made executable, so a
/// binary swapped in after the `cat` never runs. Drives the generated
/// script the way the remote's `sh` would.
#[test]
fn the_upload_is_checked_by_the_shell_before_it_can_run() {
    let t = acs::testutil::TempDir::new();
    let file = t.path().join("acs.new.abcd");
    std::fs::write(&file, b"the real acs").unwrap();
    let want = acs::sha256::hex(&acs::sha256::digest(b"the real acs"));
    let quoted = format!("'{}'", file.display());

    // The honest upload passes and the script carries on.
    let script = format!("{} echo passed", acs::install::check_upload(&quoted, &want));
    let out = output_of(Command::new("/bin/sh").arg("-c").arg(&script));
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("passed"));

    // Someone replaces the file between the cat and the exec.
    std::fs::write(&file, b"not the real acs").unwrap();
    let out = output_of(Command::new("/bin/sh").arg("-c").arg(&script));
    assert!(!out.status.success(), "a swapped binary was accepted");
    assert_eq!(out.status.code(), Some(5));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("does not match the checksum"), "{err}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("passed"));
    // And it is gone, so nothing can exec it later.
    assert!(!file.exists(), "the rejected upload was left behind");
}

/// acs-4km: a host with no way to hash says so, and still refuses to run
/// what it could not check.
#[test]
fn a_host_with_no_checksum_tool_refuses_the_install() {
    let t = acs::testutil::TempDir::new();
    let file = t.path().join("acs.new.abcd");
    std::fs::write(&file, b"x").unwrap();
    let quoted = format!("'{}'", file.display());
    let script = format!(
        "{} echo passed",
        acs::install::check_upload(&quoted, &acs::sha256::hex(&acs::sha256::digest(b"x")))
    );
    // An empty PATH leaves sha256sum, shasum and openssl all unfindable.
    let out = output_of(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("PATH", "/nonexistent"),
    );
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sha256sum, shasum or openssl"));
    // It never got as far as `echo passed`, so the file was never made
    // executable and never ran — which is the property that matters. (The
    // cleanup `rm` cannot run either with this PATH; the leftover is inert.)
    assert!(!String::from_utf8_lossy(&out.stdout).contains("passed"));
}

/// acs-4km: the payload trailer is checked against the digest the client
/// computed, not merely parsed. A well-formed blob is not necessarily ours.
#[test]
fn finisher_rejects_a_payload_blob_that_does_not_match() {
    let remote = Remote::new();
    let dir = remote
        .home()
        .join(format!(".local/share/acs/{}", acs::VERSION));
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join("acs.new.00ff");
    std::fs::copy(exe(), &tmp).unwrap();
    let slim = acs::sha256::hex(&acs::sha256::digest(&std::fs::read(&tmp).unwrap()));
    let mut child = Command::new(&tmp)
        .args([
            "_install",
            "--finish",
            "--token",
            "00ff",
            "--slim-sha256",
            &slim,
            "--blob-sha256",
            &"0".repeat(64),
            "--payloads",
        ])
        .env("HOME", remote.home())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"not the payload set we sent")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("checksum") || err.contains("corrupt"), "{err}");
    assert!(!dir.join("acs").exists(), "a bad blob was installed");
}
