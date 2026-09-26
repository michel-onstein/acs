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
case "$(basename "$0") $1 $2" in
    "docker port "*) echo 127.0.0.1:49999 ;;
    "ssh-keygen "*) while [ $# -gt 1 ]; do shift; done; : >"$1"; : >"$1.pub" ;;
    # `cargo xtask dist` leaves a binary per target and the source stamp.
    "cargo xtask dist")
        for t in aarch64-apple-darwin x86_64-apple-darwin \
            x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
            mkdir -p "dist/$t" && : >"dist/$t/acs"
        done
        sh scripts/source_stamp.sh >dist/source.stamp
        ;;
    "cargo "*) echo "cargo-env port=$ACS_E2E_PORT container=$ACS_E2E_CONTAINER" >>"$FAKE_LOG" ;;
esac
"#;

/// The target triple `e2e_ssh.sh` picks for the host running these tests,
/// and so the client binary it insists on.
fn host_target() -> &'static str {
    match (cfg!(target_os = "macos"), cfg!(target_arch = "aarch64")) {
        (true, true) => "aarch64-apple-darwin",
        (true, false) => "x86_64-apple-darwin",
        (false, true) => "aarch64-unknown-linux-musl",
        (false, false) => "x86_64-unknown-linux-musl",
    }
}

/// A copy of the repository's scripts at `<dir>/<name>`, as a checkout, with
/// a source tree and a `dist/` built from it.
fn checkout(dir: &Path, name: &str) -> PathBuf {
    let root = bare_checkout(dir, name);
    dist(&root);
    root
}

/// The same, with no `dist/` at all — a fresh worktree.
fn bare_checkout(dir: &Path, name: &str) -> PathBuf {
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
    root
}

/// What `cargo xtask dist` leaves behind for `--no-build`: a binary per
/// target, and the fingerprint of the checkout's sources as they are now.
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
    for t in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ] {
        std::fs::create_dir_all(root.join("dist").join(t)).unwrap();
        std::fs::write(root.join("dist").join(t).join("acs"), "").unwrap();
    }
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

/// acs-0pr: absent is not stale. A fresh worktree — where nearly all work in
/// this repo happens — has no `dist/` at all, and there is nothing there to
/// be misled by, so `--no-build` builds it once and says so instead of
/// refusing a command the caller would only re-run without the flag.
#[test]
fn e2e_no_build_builds_a_dist_that_is_not_there() {
    let dir = TempDir::new();
    let root = bare_checkout(dir.path(), "a");
    let out = try_run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "refused an absent dist/: {err}");
    assert!(err.contains("no dist/ yet"), "said nothing: {err}");
    assert!(!err.contains("REFUSING"), "{err}");
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
    assert!(log.contains("cargo xtask dist"), "{log}");
    // And having built it, it ran the suite against it.
    assert!(log.contains("docker run"), "{log}");
    assert!(log.contains("cargo test"), "{log}");
    // The stamp that build left makes the next run a reuse, not a rebuild.
    let again = run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    assert!(
        !again.iter().any(|l| l.starts_with("cargo xtask")),
        "{again:?}"
    );
}

/// acs-0pr: a `dist/` built with `--targets` that leaves this host out
/// passes the stamp check — it really was built from this tree — and used to
/// fail much later, inside the harness, on a missing file.
#[test]
fn e2e_refuses_a_dist_without_a_client_for_this_host() {
    let dir = TempDir::new();
    let root = checkout(dir.path(), "a");
    let client = root.join("dist").join(host_target()).join("acs");
    std::fs::remove_file(&client).unwrap();

    for args in [
        &["--no-build"][..],
        &["--no-build", "--allow-stale-dist"][..],
    ] {
        let out = try_run(dir.path(), &root, "e2e_ssh.sh", args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "ran without a client: {err}");
        // Named, with a way to get it.
        assert!(err.contains(&client.display().to_string()), "{err}");
        assert!(err.contains("cargo xtask dist"), "{err}");
        // Nothing was started to fail obscurely later.
        let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
        assert!(!log.contains("docker run"), "{log}");
        assert!(!log.contains("cargo test"), "{log}");
    }
}

/// acs-0pr: `--allow-stale-dist` only governs how `dist/` is reused, so
/// without `--no-build` it did nothing at all. A mistyped invocation should
/// say so rather than quietly rebuild.
#[test]
fn e2e_allow_stale_dist_needs_no_build() {
    let dir = TempDir::new();
    let root = checkout(dir.path(), "a");
    let out = try_run(dir.path(), &root, "e2e_ssh.sh", &["--allow-stale-dist"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("--allow-stale-dist does nothing"), "{err}");
    assert!(err.contains("usage:"), "{err}");
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    assert!(log.is_empty(), "{log}");
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

/// acs-o9v, acs-x57: the installer in the repository carries acs's own
/// release key and acs's own releases, and checks the signature before it
/// reads the checksums. Two copies of a key drift; this is the guard
/// against that.
///
/// These are the *source* script's values — what someone who fetches
/// `scripts/install.sh` from this repository gets. The copy published with
/// a release is not this file: `cargo xtask package` substitutes the
/// build's URL and key into it, and the other half of this guard —
/// `package::the_packaged_installer_carries_this_builds_url_and_key` —
/// holds that copy against the binary for *every* build, a fork's
/// included, which this test cannot do from here.
#[test]
fn the_installer_carries_the_same_release_key_and_checks_before_reading() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .unwrap();
    // Both values sit in one single-quoted assignment at the start of a
    // line, which is what lets them be substituted; `install_script`
    // refuses to package a script where that has stopped being true.
    let value = |name: &str| {
        let prefix = format!("{name}='");
        script
            .lines()
            .find_map(|l| l.strip_prefix(&prefix)?.strip_suffix('\''))
            .unwrap_or_else(|| panic!("the installer has no single-quoted {name} assignment"))
    };
    assert_eq!(
        value("release_key"),
        acs::signature::UPSTREAM_RELEASE_KEY,
        "the installer's release key is not acs's own"
    );
    assert_eq!(
        value("default_releases"),
        acs::release::UPSTREAM_RELEASES_URL,
        "the installer's default releases are not acs's own"
    );
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

/// A real git repository at `<dir>/notes` holding `release-changes.sh`, with
/// one empty commit per subject in `history` (oldest first), and a tag
/// wherever an entry is a `vX.Y.Z` instead of a subject.
fn notes_checkout(dir: &Path, history: &[&str]) -> PathBuf {
    let root = dir.join("notes");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    let dest = root.join("scripts/release-changes.sh");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/release-changes.sh"),
        &dest,
    )
    .unwrap();
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    for h in history {
        let tag = h
            .strip_prefix('v')
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
        if tag {
            git(&["tag", h]);
        } else {
            git(&["commit", "-q", "--allow-empty", "-m", h]);
        }
    }
    root
}

/// The changes `scripts/release-changes.sh` lists for `tag`.
fn changes(root: &Path, tag: &str) -> Vec<String> {
    let out = Command::new("sh")
        .arg(root.join("scripts/release-changes.sh"))
        .arg(tag)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "release-changes.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// The notes tell someone about to install the binary what changed in it, so
/// the bookkeeping stays out: the release's own version bump, and the
/// `chore(beads):` commits that only open and close issues under `.beads/`.
/// Every other `chore` is a change to the build or the scripts, and stays.
#[test]
fn release_notes_leave_out_the_bookkeeping() {
    let dir = TempDir::new();
    let root = notes_checkout(
        dir.path(),
        &[
            "feat: the first thing",
            "v0.1.0",
            "feat(timing): name the greeting as its own phase",
            "chore(beads): close acs-ftn, file the phase-name drift",
            "fix(e2e): check the client binary",
            "chore(ci): pin the runner image",
            "chore(release): v0.2.0",
            "v0.2.0",
        ],
    );
    assert_eq!(
        changes(&root, "v0.2.0"),
        [
            "- chore(ci): pin the runner image",
            "- fix(e2e): check the client binary",
            "- feat(timing): name the greeting as its own phase",
        ],
        "a beads chore, the release chore, or an ordinary chore, is in or out wrongly"
    );
}

/// With no previous tag the whole history is the release — still without the
/// bookkeeping — and the list says as much.
#[test]
fn a_first_release_says_so_and_still_filters() {
    let dir = TempDir::new();
    let root = notes_checkout(
        dir.path(),
        &[
            "feat: the first thing",
            "chore(beads): file acs-aaa",
            "v0.1.0",
        ],
    );
    assert_eq!(
        changes(&root, "v0.1.0"),
        ["First release.", "", "- feat: the first thing"]
    );
}

/// A release that is nothing but bookkeeping lists nothing, rather than
/// failing: the `grep` matching no line must not end the script (`set -e`).
#[test]
fn a_release_of_only_bookkeeping_lists_nothing() {
    let dir = TempDir::new();
    let root = notes_checkout(
        dir.path(),
        &[
            "feat: the first thing",
            "v0.1.0",
            "chore(beads): close acs-ftn",
            "chore(release): v0.2.0",
            "v0.2.0",
        ],
    );
    assert!(changes(&root, "v0.2.0").is_empty());
}
