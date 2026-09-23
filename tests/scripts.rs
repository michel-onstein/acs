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

/// A copy of the repository's scripts at `<dir>/<name>`, as a checkout, with
/// a source tree and a `dist/` built from it.
fn checkout(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    let scripts = root.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for s in ["test_linux.sh", "e2e_ssh.sh", "source_stamp.sh"] {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join(s);
        let dest = scripts.join(s);
        std::fs::copy(src, &dest).unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"acs\"\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    dist(&root);
    root
}

/// What `cargo xtask dist` leaves behind for `--no-build`: the fingerprint of
/// the checkout's sources as they are now, beside the binaries.
fn dist(root: &Path) -> String {
    let out = Command::new("sh")
        .arg(root.join("scripts").join("source_stamp.sh"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "source_stamp.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stamp = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stamp.trim().len(), 64, "not a sha256: {stamp:?}");
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist").join("source.stamp"), &stamp).unwrap();
    stamp
}

/// Runs `script` of `root` with the fakes first on PATH; returns the log.
fn run(dir: &Path, root: &Path, script: &str, args: &[&str]) -> Vec<String> {
    let out = try_run(dir, root, script, args);
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(dir.join("log"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// The same, without insisting the script succeeded.
fn try_run(dir: &Path, root: &Path, script: &str, args: &[&str]) -> std::process::Output {
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
    Command::new("sh")
        .arg(root.join("scripts").join(script))
        .args(args)
        .env("PATH", path)
        .env("FAKE_LOG", &log)
        .env_remove("ACS_E2E_PORT")
        .output()
        .unwrap()
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

/// acs-gb4: `--no-build` reuses whatever is in `dist/`, and a red e2e run
/// from a binary two edits old reads exactly like a real one. It was found
/// on an *uncommitted* edit — build dist/, edit a source file, run again —
/// where HEAD never moves and a dirty flag is set both times, so the stamp
/// has to be over the file contents. The run must refuse, unless asked for
/// the stale binaries by name.
#[test]
fn e2e_no_build_refuses_a_dist_that_is_not_this_source_tree() {
    let dir = TempDir::new();
    let root = checkout(dir.path(), "a");
    // A dist/ built from this tree is reused, as before.
    let fresh = run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    assert!(
        fresh.iter().any(|l| l.starts_with("docker run")),
        "{fresh:?}"
    );

    // The edit that bit: nothing committed, nothing but a file's contents.
    std::fs::write(root.join("src/main.rs"), "fn main() { broken() }\n").unwrap();
    let out = try_run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "reused a stale dist/: {err}");
    assert!(err.contains("REFUSING --no-build"), "{err}");
    assert!(err.contains("--allow-stale-dist"), "no way out: {err}");
    // And it stopped before a host was started, let alone a test run.
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    assert!(!log.contains("docker run"), "{log}");
    assert!(!log.contains("cargo test"), "{log}");

    // Asked for by name, the stale dist/ is used.
    let stale = run(
        dir.path(),
        &root,
        "e2e_ssh.sh",
        &["--no-build", "--allow-stale-dist"],
    );
    assert!(
        stale.iter().any(|l| l.starts_with("docker run")),
        "{stale:?}"
    );

    // Rebuilding dist/ makes --no-build honest again.
    dist(&root);
    run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);

    // A dist/ with no stamp at all — an older or a half-written one — is a
    // mismatch too.
    std::fs::remove_file(root.join("dist").join("source.stamp")).unwrap();
    let out = try_run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    assert!(!out.status.success(), "reused an unstamped dist/");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("<none>"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A checkout whose `scripts/` holds version-bump.sh, a release-binaries.sh
/// that only logs, and a fake `git`/`cargo` pair driven by tag files:
/// `local` is what the checkout has, `remote` what origin has, and a bump
/// appends `bumped` to both.
fn bump_checkout(dir: &Path, local: &[&str], remote: &[&str], bumped: &str) -> PathBuf {
    let root = dir.join("repo");
    let scripts = root.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/version-bump.sh"),
        scripts.join("version-bump.sh"),
    )
    .unwrap();
    let write = |path: PathBuf, body: &str| {
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write(
        scripts.join("release-binaries.sh"),
        "#!/bin/sh\necho \"release-binaries $*\" >>\"$FAKE_LOG\"\n",
    );
    // Every file ends in a newline: the fakes append to them.
    let lines = |tags: &[&str]| tags.iter().map(|t| format!("{t}\n")).collect::<String>();
    std::fs::write(dir.join("local"), lines(local)).unwrap();
    std::fs::write(dir.join("remote"), lines(remote)).unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write(
        bin.join("git"),
        &format!(
            "#!/bin/sh\necho \"git $*\" >>\"$FAKE_LOG\"\n\
             case \"$1 $2\" in\n\
             \"fetch --tags\") cat '{remote}' >> '{local}' ;;\n\
             \"tag --list\") sort -u '{local}' | grep -v '^$' ;;\n\
             esac\n",
            remote = dir.join("remote").display(),
            local = dir.join("local").display(),
        ),
    );
    write(
        bin.join("cargo"),
        &format!(
            "#!/bin/sh\necho \"cargo $*\" >>\"$FAKE_LOG\"\n\
             cat '{remote}' >> '{local}'\n\
             echo '{bumped}' >> '{local}'\n\
             echo '{bumped}' >> '{remote}'\n",
            remote = dir.join("remote").display(),
            local = dir.join("local").display(),
        ),
    );
    root
}

fn run_bump(dir: &Path, root: &Path) -> Vec<String> {
    let log = dir.join("log");
    let _ = std::fs::remove_file(&log);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let out = Command::new("sh")
        .arg(root.join("scripts/version-bump.sh"))
        .env("PATH", path)
        .env("FAKE_LOG", &log)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "version-bump.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// Regression (acs-zc4): a tag origin already has, but this checkout does
/// not, is not published again — only the tag the bump made is.
#[test]
fn version_bump_only_releases_the_tag_it_made() {
    let dir = TempDir::new();
    let root = bump_checkout(dir.path(), &["v0.1.0"], &["v0.1.0", "v0.2.0"], "v0.3.0");
    let log = run_bump(dir.path(), &root);
    let released: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("release-binaries"))
        .collect();
    assert_eq!(released, ["release-binaries v0.3.0"], "{log:?}");
    // The tags are fetched before they are snapshotted.
    let fetch = log.iter().position(|l| l.starts_with("git fetch --tags"));
    let list = log.iter().position(|l| l.starts_with("git tag --list"));
    assert!(fetch < list, "fetch after the snapshot: {log:?}");
}

/// A bump that releases nothing (nothing unreleased) publishes nothing.
#[test]
fn version_bump_without_a_new_tag_releases_nothing() {
    let dir = TempDir::new();
    let root = bump_checkout(dir.path(), &["v0.1.0"], &["v0.1.0", "v0.2.0"], "");
    let log = run_bump(dir.path(), &root);
    assert!(
        !log.iter().any(|l| l.starts_with("release-binaries")),
        "{log:?}"
    );
}

/// acs-g3j: the installer takes the version out of `SHA256SUMS`, and the
/// old pattern allowed `/` and `..` anywhere in the archive name. A
/// channel under someone else's control could therefore name a version
/// that walks out of the temporary directory and out of `$lib` — which the
/// root branch writes as root. The Rust side was always strict; the two
/// now agree.
#[test]
fn the_installer_takes_only_an_x_y_z_version_from_the_sums() {
    let t = TempDir::new();
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .unwrap();
    // The line the script greps SHA256SUMS with, lifted from the script so
    // the test cannot drift from it.
    let pattern = script
        .lines()
        .find(|l| l.contains("grep -E") && l.contains("SHA256SUMS"))
        .expect("the sums grep");
    let pattern = pattern
        .split('"')
        .nth(1)
        .expect("the quoted pattern")
        // Undo what the shell would do to the double-quoted string before
        // grep ever sees it.
        .replace("\\\\", "\\")
        .replace("\\$", "$")
        .replace("$target", "x86_64-unknown-linux-musl");

    let sums = t.path().join("SHA256SUMS");
    let hash = "0".repeat(64);
    let check = |name: &str| {
        std::fs::write(&sums, format!("{hash}  {name}\n")).unwrap();
        let out = Command::new("grep")
            .arg("-E")
            .arg(&pattern)
            .arg(&sums)
            .output()
            .unwrap();
        out.status.success()
    };

    assert!(
        check("acs-1.2.3-x86_64-unknown-linux-musl.tar.gz"),
        "a real release"
    );
    assert!(
        check("acs-0.11.3-x86_64-unknown-linux-musl.tar.gz"),
        "a real release"
    );
    for bad in [
        "acs-1.0/../../../etc/cron.d/x-x86_64-unknown-linux-musl.tar.gz",
        "acs-1.0/2.3-x86_64-unknown-linux-musl.tar.gz",
        "acs-1.2.3-rc1-x86_64-unknown-linux-musl.tar.gz",
        "acs-1.2-x86_64-unknown-linux-musl.tar.gz",
        "acs-..-x86_64-unknown-linux-musl.tar.gz",
    ] {
        assert!(!check(bad), "accepted {bad}");
    }
}

/// acs-o9v: the installer carries the same release key as the binary, and
/// checks the signature before it reads the checksums. Two copies of a key
/// drift; this is the guard against that.
#[test]
fn the_installer_carries_the_same_release_key_and_checks_before_reading() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .unwrap();
    let key = script
        .lines()
        .find_map(|l| l.trim().strip_prefix("release_key="))
        .expect("the installer names a release key")
        .trim_matches('"')
        .to_string();
    // The script is copied into each release as it is, so it carries acs's
    // own key rather than this build's: a fork edits that line as it edits
    // the script's default releases URL (acs-ktm, VERSIONING.md
    // "Forking"). In every build but a fork's the two are one key.
    assert_eq!(
        key,
        acs::signature::UPSTREAM_RELEASE_KEY,
        "the installer's release key is not acs's own"
    );
    if acs::signature::RELEASE_KEY == acs::signature::UPSTREAM_RELEASE_KEY {
        assert_eq!(
            key,
            acs::signature::RELEASE_KEY,
            "the installer's release key is not the one built into acs"
        );
    }
    // The identity and the namespace must match too, or a signature this
    // binary accepts is one the installer rejects.
    assert!(
        script.contains(&format!("-I {}", acs::signature::IDENTITY)),
        "the installer does not verify as {}",
        acs::signature::IDENTITY
    );
    assert!(
        script.contains(&format!("-n {}", acs::signature::NAMESPACE)),
        "the installer does not verify in the {} namespace",
        acs::signature::NAMESPACE
    );
    // The check comes before the checksums are read: the grep that picks the
    // archive out must sit after the ssh-keygen that verifies them.
    let verify = script.find("ssh-keygen -Y verify").expect("a verify");
    let read = script
        .find("grep -E \"^[0-9a-f]{64}")
        .expect("the sums grep");
    assert!(
        verify < read,
        "the installer reads SHA256SUMS before checking its signature"
    );
}
