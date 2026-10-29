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
    upgrade_under(path, home, server, args, None)
}

/// Make `cmd` run under the umask `mask`, as a login shell with that umask
/// would start it. Set in the child alone: the umask belongs to the whole
/// test process, shared by every test running beside this one.
fn with_umask(cmd: &mut Command, mask: u32) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: umask is async-signal-safe and cannot fail.
    unsafe {
        cmd.pre_exec(move || {
            libc::umask(mask as libc::mode_t);
            Ok(())
        })
    }
}

/// [`upgrade`], under the umask `mask` when one is given.
fn upgrade_under(
    path: &Path,
    home: &Path,
    server: &ReleaseServer,
    args: &[&str],
    mask: Option<u32>,
) -> (i32, String, String) {
    let mut cmd = Command::new(path);
    if let Some(mask) = mask {
        with_umask(&mut cmd, mask);
    }
    upgrade_with(cmd, home, server, args)
}

/// [`upgrade`] through `cmd`, a `Command` for the acs to run, set up as the
/// test needs.
fn upgrade_with(
    mut cmd: Command,
    home: &Path,
    server: &ReleaseServer,
    args: &[&str],
) -> (i32, String, String) {
    let out = output_of(
        cmd.arg("upgrade")
            // The test server speaks http, which acs-95w refuses without
            // an explicit say-so on the command line.
            .arg("--allow-insecure-url")
            .args(args)
            .env("ACS_RELEASES_URL", &server.url)
            // The server signs its releases with a key of its own, which
            // acs honours only because the channel is not the default one
            // (acs-o9v).
            .env("ACS_RELEASE_KEY", server.release_key())
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
    // A release after 0.18.0 is fetched from its bare tag.
    let archive = format!("/download/{NEW}/");
    assert!(
        server.hits().iter().any(|h| h.contains(&archive)),
        "{:?}",
        server.hits()
    );
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
    // acs-hofu.5: an install from before acscp gains the relative link,
    // which already runs the new version (ACSCP_DESIGN §9).
    let copy = link.with_file_name(acs::layout::COPY_NAME);
    assert_eq!(
        std::fs::read_link(&copy).unwrap(),
        std::path::Path::new("acs")
    );
    assert!(version_of(&copy).starts_with(&format!("acs {NEW} ")));
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

/// acs-o9v: the checksums are believed only when the release key signed
/// them. Both halves of the attack the signature is for — a release whose
/// files were replaced by someone who could write to it, and one published
/// with no signature at all — are refused before anything is downloaded,
/// unpacked or run.
#[test]
fn a_release_not_signed_by_the_release_key_is_refused() {
    // Signed, but by a key that is not ours.
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    server.sign_with_another_key();
    let acs = plain_copy(d.path());
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("is not signed by the acs release key"),
        "{err}"
    );
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
    // The archive was never fetched: the refusal comes before the download.
    assert!(
        !server.hits().iter().any(|h| h.ends_with(".tar.gz")),
        "{:?}",
        server.hits()
    );

    // Published with no signature.
    let d = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    server.remove("/latest/download/SHA256SUMS.sig");
    let acs = plain_copy(d.path());
    let (code, _, err) = upgrade(&acs, d.path(), &server, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("no signature for SHA256SUMS"), "{err}");
    assert_eq!(std::fs::read(&acs).unwrap(), std::fs::read(exe()).unwrap());
    assert!(
        !server.hits().iter().any(|h| h.ends_with(".tar.gz")),
        "{:?}",
        server.hits()
    );
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
    // A release up to 0.18.0 is fetched from its `v` tag.
    assert!(
        server
            .hits()
            .iter()
            .any(|h| h.contains("/download/v0.0.1/")),
        "{:?}",
        server.hits()
    );
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
    assert_eq!(
        server.hits(),
        // The signature is fetched too, and checked before the sums
        // are read (acs-o9v).
        [
            "/latest/download/SHA256SUMS",
            "/latest/download/SHA256SUMS.sig"
        ]
    );
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
    assert_eq!(
        server.hits(),
        // The signature is fetched too, and checked before the sums
        // are read (acs-o9v).
        [
            "/latest/download/SHA256SUMS",
            "/latest/download/SHA256SUMS.sig"
        ]
    );
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

/// acs-95w: without the flag, `acs upgrade` refuses a releases URL it
/// cannot authenticate rather than downloading and running what it finds.
#[test]
fn an_insecure_releases_url_is_refused_without_the_flag() {
    let t = TempDir::new();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);
    let bin = t.path().join("acs");
    std::fs::copy(exe(), &bin).unwrap();
    let out = output_of(
        Command::new(&bin)
            .arg("upgrade")
            .arg("--check")
            .env("ACS_RELEASES_URL", &server.url)
            .env("HOME", t.path())
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
    );
    assert!(!out.status.success(), "an http mirror was accepted");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot be authenticated"), "{err}");
    assert!(err.contains("--allow-insecure-url"), "{err}");
}

/// The mode bits of `path`, without the file type.
fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// acs-91q2: under umask 002 the new version's directory was made 0775, and
/// a client using the upgraded host as a remote refused to run from it
/// ("is writable by others") and uploaded the same version again.
#[test]
fn a_versioned_upgrade_under_a_group_writable_umask_makes_its_directory_0755() {
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
    let (code, _, err) = upgrade_under(&link, &home, &server, &[], Some(0o002));
    assert_eq!(code, 0, "{err}");
    let new = share.join(NEW).join("acs");
    assert!(version_of(&new).starts_with(&format!("acs {NEW} ")));
    assert_eq!(mode_of(new.parent().unwrap()), 0o755);
    assert_eq!(mode_of(&new), 0o755);
}

/// acs-3sae: the scratch directory the release is downloaded, verified and
/// unpacked in was made 0777 & ~umask — 0775 under umask 002, so anyone in
/// the group could swap the archive between its check and `tar`. The new
/// binary's "it runs" check happens while the directory still exists, so
/// the fake release records the directory's mode from there.
#[test]
fn the_scratch_directory_is_private_under_a_group_writable_umask() {
    let d = TempDir::new();
    let tmp = d.path().join("tmp");
    std::fs::create_dir(&tmp).unwrap();
    let log = d.path().join("scratch-modes");
    let server = ReleaseServer::start();
    let fake = format!(
        "#!/bin/sh\necho 'acs {NEW} (protocol 9, {t})'\nls -ld '{tmp}'/acs-upgrade.* >> '{log}'\n",
        t = acs::payload::OWN_TARGET,
        tmp = tmp.display(),
        log = log.display()
    );
    server.release(NEW, fake.as_bytes(), true, false);
    let acs = plain_copy(d.path());
    let mut cmd = Command::new(&acs);
    with_umask(&mut cmd, 0o002).env("TMPDIR", &tmp);
    let (code, _, err) = upgrade_with(cmd, d.path(), &server, &[]);
    assert_eq!(code, 0, "{err}");
    let modes = std::fs::read_to_string(&log).unwrap();
    let first = modes.lines().next().expect("the check ran the binary");
    assert!(
        first.starts_with("drwx------"),
        "scratch directory: {first}"
    );
    // And it is gone afterwards.
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
}

/// acs-91q2: the one-line installer under umask 002 left
/// `~/.local/share/acs/<version>/` group-writable, so the remote prelude
/// refused it and the client uploaded the version it was installed to save.
/// Run against this server by giving a copy of the script the server's
/// key, as `cargo xtask package` does for a fork.
#[test]
fn the_installer_under_a_group_writable_umask_makes_its_directories_0755() {
    // As root it installs into /usr/local, which a test must not touch;
    // the directories it makes there come from the same lines.
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root");
        return;
    }
    let d = TempDir::new();
    let home = d.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = ReleaseServer::start();
    server.release(NEW, &ReleaseServer::fake_acs(NEW), true, false);

    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .unwrap();
    let key_line = source
        .lines()
        .find(|l| l.starts_with("release_key='"))
        .expect("the installer's release key");
    let script = d.path().join("install.sh");
    let ours = format!("release_key='{}'", server.release_key());
    std::fs::write(&script, source.replace(key_line, &ours)).unwrap();

    let mut cmd = Command::new("sh");
    with_umask(&mut cmd, 0o002);
    let out = output_of(
        cmd.arg(&script)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TMPDIR", d.path())
            .env("ACS_RELEASES_URL", &server.url)
            .env("ACS_ALLOW_INSECURE_URL", "1"),
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");

    let lib = home.join(".local/share/acs");
    let installed = lib.join(NEW).join("acs");
    assert!(version_of(&installed).starts_with(&format!("acs {NEW} ")));
    assert_eq!(mode_of(&lib), 0o755);
    assert_eq!(mode_of(installed.parent().unwrap()), 0o755);
    assert_eq!(mode_of(&installed), 0o755);
    // The prelude checks the link's directory too, on the way through it.
    assert_eq!(mode_of(&home.join(".local/bin")), 0o755);
}
