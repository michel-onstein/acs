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

/// What `test_linux.sh` would do in its container: the `docker run` argv,
/// boundaries kept, and the `cargo` calls its inner script makes when run
/// here against fake `apk`, `adduser`, `chmod` and `cargo`.
struct LinuxRun {
    docker: Vec<String>,
    cargo: Vec<String>,
}

impl LinuxRun {
    fn volume(&self) -> &str {
        self.docker
            .iter()
            .find(|a| a.starts_with("acs-linux-target"))
            .expect("a target volume")
    }
}

const LOGGER: &str = r#"#!/bin/sh
echo "$(basename "$0") $* [mu=${ACS_MULTIUSER_TEST:-} ns=${ACS_NETNS_TEST:-}]" >>"$FAKE_LOG"
"#;

/// `test_linux.sh args` against a docker that only keeps its argv; `Err`
/// with the script's output when it refused before reaching docker.
fn try_linux_run(dir: &Path, root: &Path, args: &[&str]) -> Result<LinuxRun, std::process::Output> {
    let bin = dir.join("linux-bin");
    if !bin.exists() {
        std::fs::create_dir(&bin).unwrap();
        let fakes = [
            ("docker", "#!/bin/sh\nprintf '%s\\0' \"$@\" >\"$ARGV\"\n"),
            ("apk", LOGGER),
            ("adduser", LOGGER),
            ("chmod", LOGGER),
            ("cargo", LOGGER),
        ];
        for (p, body) in fakes {
            let f = bin.join(p);
            std::fs::write(&f, body).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let (argv, log) = (dir.join("argv"), dir.join("linux-log"));
    let _ = std::fs::remove_file(&argv);
    let _ = std::fs::remove_file(&log);
    let out = Command::new("sh")
        .arg(root.join("scripts/test_linux.sh"))
        .args(args)
        .env("PATH", &path)
        .env("ARGV", &argv)
        .output()
        .unwrap();
    if !out.status.success() {
        assert!(!argv.exists(), "refused, but only after calling docker");
        return Err(out);
    }
    let docker: Vec<String> = std::fs::read_to_string(&argv)
        .unwrap()
        .split('\0')
        .map(String::from)
        .collect();
    let env: Vec<(String, String)> = docker
        .windows(2)
        .filter(|w| w[0] == "-e")
        .map(|w| {
            let (k, v) = w[1].split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect();
    let i = docker.iter().position(|a| a == "-euc").expect("sh -euc");
    // The script, then `$0` and the positionals; the last split is the
    // empty string after the final NUL.
    let rest = &docker[i + 1..docker.len() - 1];
    let inner = Command::new("sh")
        .arg("-euc")
        .args(rest)
        .envs(env)
        .env("PATH", &path)
        .env("FAKE_LOG", &log)
        .output()
        .unwrap();
    assert!(
        inner.status.success(),
        "the container's script: {}",
        String::from_utf8_lossy(&inner.stderr)
    );
    let cargo = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("cargo "))
        .map(String::from)
        .collect();
    Ok(LinuxRun { docker, cargo })
}

fn linux_run(dir: &Path, root: &Path, args: &[&str]) -> LinuxRun {
    try_linux_run(dir, root, args).unwrap_or_else(|out| {
        panic!(
            "test_linux.sh {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// A checkout with `tests/<name>.rs` for each name, for `--only` to find.
fn linux_checkout(dir: &Path, tests: &[&str]) -> PathBuf {
    let root = bare_checkout(dir, "a");
    std::fs::create_dir_all(root.join("tests")).unwrap();
    for t in tests {
        std::fs::write(root.join("tests").join(format!("{t}.rs")), "").unwrap();
    }
    root
}

const MULTIUSER: &str = "cargo test -p acs --test multiuser -- --include-ignored \
    --skip peer_helper --test-threads=1";
const NETNS: &str = "cargo test -p acs --test netns -- --include-ignored \
    --test-threads=1 --nocapture";

#[test]
fn test_linux_with_no_arguments_runs_all_three_steps() {
    let dir = TempDir::new();
    let root = linux_checkout(dir.path(), &[]);
    assert_eq!(
        linux_run(dir.path(), &root, &[]).cargo,
        [
            "cargo test -p acs [mu= ns=]".to_string(),
            format!("{MULTIUSER} [mu=1 ns=]"),
            format!("{NETNS} [mu= ns=1]"),
        ]
    );
}

/// acs-pjk: `--only` runs one test target, in the same container with the
/// same volumes, and a privileged one keeps its step's environment and
/// flags — so iterating on it is iterating on the real thing.
#[test]
fn test_linux_only_runs_one_target_as_the_full_run_would() {
    let dir = TempDir::new();
    let root = linux_checkout(dir.path(), &["netns", "multiuser", "client"]);
    let full = linux_run(dir.path(), &root, &[]);
    let cases: [(&[&str], String); 5] = [
        (&["--only", "netns"], format!("{NETNS} [mu= ns=1]")),
        (
            &["--only", "netns", "--", "address"],
            format!("{NETNS} address [mu= ns=1]"),
        ),
        (&["--only", "multiuser"], format!("{MULTIUSER} [mu=1 ns=]")),
        (
            &["--only", "client", "--", "--exact", "a b"],
            "cargo test -p acs --test client -- --exact a b [mu= ns=]".to_string(),
        ),
        (
            &["--platform", "linux/amd64", "--only", "lib"],
            "cargo test -p acs --lib -- [mu= ns=]".to_string(),
        ),
    ];
    for (args, want) in cases {
        let only = linux_run(dir.path(), &root, args);
        assert_eq!(only.cargo, [want], "{args:?}");
        assert_eq!(only.volume(), full.volume(), "{args:?}");
    }
    // `a b` stayed one argument all the way to the harness.
    let only = linux_run(dir.path(), &root, &["--only", "client", "--", "a b"]);
    assert!(only.docker.iter().any(|a| a == "a b"), "{:?}", only.docker);
}

#[test]
fn test_linux_refuses_a_bad_selection_before_starting_docker() {
    let dir = TempDir::new();
    let root = linux_checkout(dir.path(), &["netns"]);
    for (args, said) in [
        (&["--only", "nosuch"][..], "no tests/nosuch.rs"),
        (&["--only", "../netns"], "takes a test name"),
        (&["--only"], "usage"),
        (&["--", "address"], "need --only"),
        (&["--bogus"], "unknown argument"),
    ] {
        let out = try_linux_run(dir.path(), &root, args)
            .err()
            .unwrap_or_else(|| panic!("{args:?} was accepted"));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(said), "{args:?}: {err}");
    }
}

/// acs-hdj4: the multi-user, netns and e2e tests are `#[ignore]`, so a plain
/// `cargo test` counts them rather than passing them unrun. The scripts that
/// can run them have to ask for them, or they go dark there too — counted as
/// ignored in the one place they were meant to run.
#[test]
fn the_container_scripts_run_the_ignored_tests() {
    let dir = TempDir::new();
    let root = checkout(dir.path(), "a");
    let linux = linux_run(dir.path(), &root, &[]).cargo.join("\n");
    for step in [
        "--test multiuser -- --include-ignored --skip peer_helper",
        "--test netns -- --include-ignored",
    ] {
        assert!(
            linux.contains(step),
            "test_linux.sh lacks {step:?}:\n{linux}"
        );
    }
    let e2e = run(dir.path(), &root, "e2e_ssh.sh", &["--no-build"]);
    let test = e2e
        .iter()
        .find(|l| l.starts_with("cargo test"))
        .expect("e2e_ssh.sh ran no cargo test");
    assert!(
        test.contains("--test e2e_ssh -- --include-ignored"),
        "{test}"
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
    // The bridge: origin has v-tags up to v0.18.0, the bump makes 0.19.0.
    let root = bump_checkout(dir.path(), &["v0.17.0"], &["v0.17.0", "v0.18.0"], "0.19.0");
    let log = run_bump(dir.path(), &root);
    let released: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("release-binaries"))
        .collect();
    assert_eq!(released, ["release-binaries 0.19.0"], "{log:?}");
    // Both forms are listed, so v0.18.0 is not taken for a new tag.
    assert!(
        log.iter().any(|l| l == "git tag --list v[0-9]* [0-9]*"),
        "{log:?}"
    );
    // The tags are fetched before they are snapshotted.
    let fetch = log.iter().position(|l| l.starts_with("git fetch --tags"));
    let list = log.iter().position(|l| l.starts_with("git tag --list"));
    assert!(fetch < list, "fetch after the snapshot: {log:?}");
}

/// A bump that releases nothing (nothing unreleased) publishes nothing.
#[test]
fn version_bump_without_a_new_tag_releases_nothing() {
    let dir = TempDir::new();
    let root = bump_checkout(dir.path(), &["v0.18.0"], &["v0.18.0", "0.19.0"], "");
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
/// The pattern `scripts/install.sh` greps SHA256SUMS with for `target`,
/// lifted from the script so a test cannot drift from it.
fn installer_sums_pattern(target: &str) -> String {
    let line = installer()
        .lines()
        .find(|l| l.contains("grep -E") && l.contains("SHA256SUMS"))
        .expect("the sums grep")
        .to_string();
    line.split('"')
        .nth(1)
        .expect("the quoted pattern")
        // Undo what the shell would do to the double-quoted string before
        // grep ever sees it.
        .replace("\\\\", "\\")
        .replace("\\$", "$")
        .replace("$target", target)
}

fn installer() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
        .unwrap()
}

/// Whether `pattern` matches the one-line file `line`, as `grep -E` says.
fn greps(dir: &Path, pattern: &str, line: &str) -> bool {
    let sums = dir.join("SHA256SUMS");
    std::fs::write(&sums, format!("{line}\n")).unwrap();
    Command::new("grep")
        .arg("-E")
        .arg(pattern)
        .arg(&sums)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn the_installer_takes_only_an_x_y_z_version_from_the_sums() {
    let t = TempDir::new();
    let pattern = installer_sums_pattern("x86_64-unknown-linux-musl");
    let hash = "0".repeat(64);
    let check = |name: &str| greps(t.path(), &pattern, &format!("{hash}  {name}"));

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

/// acs-sseq: the installer and acs read SHA256SUMS by one rule. The two
/// cannot share code, so the script's grep is run against
/// `release::find_asset` line by line — the separators `sha256sum` writes
/// and the ones it does not, the case of the hash, and the version shape —
/// and whichever of the installer and `acs upgrade` a release reaches, it
/// is the same archive or neither.
#[test]
fn the_installer_and_acs_read_sha256sums_alike() {
    let t = TempDir::new();
    let target = "x86_64-unknown-linux-musl";
    let pattern = installer_sums_pattern(target);
    let hash = "0123456789abcdef".repeat(4);
    let name = format!("acs-1.2.3-{target}.tar.gz");
    let lines = [
        // What sha256sum writes, in text and binary mode.
        (format!("{hash}  {name}"), true),
        (format!("{hash} *{name}"), true),
        // Runs of the two separators.
        (format!("{hash}   {name}"), true),
        (format!("{hash}  *{name}"), true),
        (format!("{hash} **{name}"), true),
        // No separator, the wrong one, or one too early.
        (format!("{hash}{name}"), false),
        (format!("{hash}\t{name}"), false),
        (format!(" {hash}  {name}"), false),
        (format!("{hash}*{name}"), false),
        // The hash: lowercase, and exactly 64 digits.
        (format!("{}  {name}", hash.to_ascii_uppercase()), false),
        (format!("{}  {name}", &hash[1..]), false),
        (format!("{hash}0  {name}"), false),
        (format!("{}  {name}", "g".repeat(64)), false),
        // The version: X.Y.Z and nothing else.
        (format!("{hash}  acs-v1.2.3-{target}.tar.gz"), false),
        (format!("{hash}  acs-1.2.3-rc1-{target}.tar.gz"), false),
        (format!("{hash}  acs-1.2-{target}.tar.gz"), false),
        (format!("{hash}  acs-1.2.3-{target}.tar.gz.sig"), false),
        (
            format!("{hash}  acs-1.2.3-aarch64-unknown-linux-musl.tar.gz"),
            false,
        ),
    ];
    for (line, want) in &lines {
        let rust = acs::release::find_asset(line, target);
        assert_eq!(rust.is_some(), *want, "acs on {line:?}: {rust:?}");
        assert_eq!(
            greps(t.path(), &pattern, line),
            *want,
            "install.sh on {line:?}"
        );
        if let Some(a) = rust {
            assert_eq!(a.file, name, "{line:?}");
            assert_eq!(a.sha256, hash, "{line:?}");
        }
    }

    // And what the script cuts out of an accepted line is what acs does.
    let cut = include_str!("../scripts/install.sh")
        .lines()
        .skip_while(|l| !l.starts_with("sum=${line%%"))
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(cut.contains("file="), "the script's cut moved:\n{cut}");
    for (line, _) in lines.iter().filter(|(_, ok)| *ok) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{cut}\nprintf '%s %s' \"$sum\" \"$file\""))
            .env("line", line)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{hash} {name}"),
            "install.sh cut {line:?}"
        );
    }
}

/// The bridge (docs/VERSIONING.md, "Tags"): the installer maps a version
/// to its release tag — `v` up to 0.18.0, bare after — exactly as
/// `release::release_tag` does. Run on the script's own function.
#[test]
fn the_installer_maps_a_version_to_its_tag_as_acs_does() {
    let script = installer();
    assert!(
        script.contains(&format!("\nlast_v_tag={}\n", acs::release::LAST_V_TAG)),
        "install.sh and release::LAST_V_TAG disagree on the cutoff"
    );
    let func = script
        .lines()
        .skip_while(|l| !l.starts_with("last_v_tag="))
        .take_while(|l| *l != "}")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        func.contains("release_tag()"),
        "the function moved:\n{func}"
    );
    for v in [
        "0.1.0", "0.9.9", "0.17.0", "0.17.99", "0.18.0", "0.18.1", "0.19.0", "0.100.0", "1.0.0",
        "1.17.0", "10.0.0",
    ] {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{func}\n}}\nrelease_tag {v}"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{v}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            acs::release::release_tag(v),
            "install.sh on {v}"
        );
    }
}

/// `ACS_VERSION` reaches the release by its tag: `0.17.0` (or `v0.17.0`, as
/// it always could be spelled) fetches from `download/v0.17.0/`, `0.19.0`
/// from `download/0.19.0/`, and anything but X.Y.Z is refused before a
/// download. Against an empty mirror, so the URL is in the failure.
#[test]
fn the_installer_fetches_a_pinned_version_from_its_tag() {
    let dir = TempDir::new();
    let releases = format!("file://{}", dir.path().display());
    let run = |want: &str| -> String {
        let out = Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
            .env("ACS_RELEASES_URL", &releases)
            .env("ACS_ALLOW_INSECURE_URL", "1")
            .env("ACS_VERSION", want)
            .env_remove("SUDO_USER")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{want}: {out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    for (want, tag) in [
        ("0.17.0", "v0.17.0"),
        ("v0.17.0", "v0.17.0"),
        ("0.18.0", "v0.18.0"),
        ("0.19.0", "0.19.0"),
        ("v0.19.0", "0.19.0"),
    ] {
        let err = run(want);
        let url = format!("cannot download {releases}/download/{tag}/SHA256SUMS");
        assert!(err.contains(&url), "{want}: {err}");
    }
    for bad in ["0.19", "0.19.0-rc1", "../0.19.0"] {
        let err = run(bad);
        assert!(err.contains("ACS_VERSION wants X.Y.Z"), "{bad}: {err}");
    }
}

/// acs-sseq: the installer lays acs out where acs looks for it. The paths
/// live in `acs::layout`; the script spells them, so this holds the two
/// together.
#[test]
fn the_installer_uses_the_layout_acs_looks_in() {
    use acs::layout::{COPY_NAME, SYSTEM_BIN, SYSTEM_ROOT, USER_BIN, USER_ROOT};
    let script = installer();
    for want in [
        format!("lib={SYSTEM_ROOT}\n"),
        format!("bin={SYSTEM_BIN}\n"),
        format!("lib=\"$HOME/{USER_ROOT}\"\n"),
        format!("bin=\"$HOME/{USER_BIN}\"\n"),
        format!("\ncopy_name={COPY_NAME}\n"),
    ] {
        assert!(script.contains(&want), "install.sh has no {want:?}");
    }
}

/// Run the installer's `link_bin` for `version` with `lib` and `bin` as
/// its directories, and say what it did: `linked`, `newer` or `theirs`.
/// The helpers are cut out of the script itself, from the first of them to
/// where it picks the directories, so the test runs exactly what ships.
fn installer_link(lib: &Path, bin: &Path, version: &str) -> String {
    installer_links(lib, bin, version).0
}

/// [`installer_link`], then the installer's `link_copy` for `bin`, as the
/// script runs them: what each did — `link_bin`'s `$linked`, and
/// `link_copy`'s `$copied` (`linked`, `theirs`, `failed` or `skipped`).
fn installer_links(lib: &Path, bin: &Path, version: &str) -> (String, String) {
    let script = installer();
    let helpers = script
        .lines()
        .skip_while(|l| !l.starts_with("rand() {"))
        .take_while(|l| !l.starts_with("if [ -n \"${ACS_INSTALL_DIR:-}\" ]"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        helpers.contains("link_bin() {") && helpers.contains("link_copy() {"),
        "the installer's link helpers moved:\n{helpers}"
    );
    // The two calls exactly as the script makes them, in that order.
    let calls = script
        .find("    link_bin \"$installed\" \"$bin/acs\"\nfi\nlink_copy \"$bin\"\n")
        .is_some();
    assert!(calls, "install.sh no longer links acscp after link_bin");
    std::fs::create_dir_all(lib.join(version)).unwrap();
    std::fs::write(lib.join(version).join("acs"), "new\n").unwrap();
    std::fs::create_dir_all(bin).unwrap();
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "set -eu\n{helpers}\nlink_bin \"$lib/$version/acs\" \"$bin/acs\"\nlink_copy \"$bin\"\nprintf '%s %s' \"$linked\" \"$copied\""
        ))
        .env("lib", lib)
        .env("bin", bin)
        .env("version", version)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = String::from_utf8(out.stdout).unwrap();
    let (linked, copied) = out.split_once(' ').unwrap();
    (linked.to_string(), copied.to_string())
}

fn copy_link(bin: &Path) -> PathBuf {
    bin.join(acs::layout::COPY_NAME)
}

/// acs-hofu.5: the installer puts `acscp -> acs`, relative, beside an
/// `acs` it linked or kept, and a re-run keeps it (ACSCP_DESIGN §9).
#[test]
fn the_installer_links_acscp_beside_its_acs() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    let links = |v| installer_links(&lib, &bin, v);
    assert_eq!(links("0.13.0"), ("linked".into(), "linked".into()));
    assert_eq!(points_at(&copy_link(&bin)), Path::new("acs"));
    // It follows acs forward with no change of its own.
    assert_eq!(links("0.14.0"), ("linked".into(), "linked".into()));
    assert_eq!(points_at(&copy_link(&bin)), Path::new("acs"));
    assert_eq!(
        std::fs::canonicalize(copy_link(&bin)).unwrap(),
        std::fs::canonicalize(lib.join("0.14.0/acs")).unwrap()
    );
    // An older ACS_VERSION keeps the link of ours, and acscp beside it.
    assert_eq!(links("0.12.0"), ("newer".into(), "linked".into()));
    assert_eq!(points_at(&copy_link(&bin)), Path::new("acs"));
}

/// acs-hofu.5: a file or link at `acscp` the installer did not make is
/// left alone, and an `acs` of the user's own gets no `acscp` at all.
#[test]
fn the_installer_leaves_a_users_acscp_alone() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(copy_link(&bin), "mine\n").unwrap();
    assert_eq!(
        installer_links(&lib, &bin, "0.13.0"),
        ("linked".into(), "theirs".into())
    );
    assert_eq!(std::fs::read_to_string(copy_link(&bin)).unwrap(), "mine\n");
    // A link of the user's to this very acs by full path is still theirs.
    std::fs::remove_file(copy_link(&bin)).unwrap();
    symlink(&bin.join("acs"), &copy_link(&bin));
    assert_eq!(
        installer_links(&lib, &bin, "0.13.0"),
        ("linked".into(), "theirs".into())
    );
    assert_eq!(points_at(&copy_link(&bin)), bin.join("acs"));

    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("acs"), "mine\n").unwrap();
    assert_eq!(
        installer_links(&lib, &bin, "0.13.0"),
        ("theirs".into(), "skipped".into())
    );
    assert!(std::fs::symlink_metadata(copy_link(&bin)).is_err());
}

/// An installed version under `lib`, as an earlier run left it.
fn installed(lib: &Path, version: &str) -> PathBuf {
    let path = lib.join(version).join("acs");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "old\n").unwrap();
    path
}

fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn points_at(link: &Path) -> PathBuf {
    std::fs::read_link(link).unwrap()
}

/// acs-feee: the installer moves `bin/acs` by `install::should_link`'s
/// rule, as acs does on a remote — a link of ours forward only, never
/// back, and nothing it did not make — instead of renaming over whatever
/// is there, which as `curl | sh` run by root replaced a user's own file.
#[test]
fn the_installer_links_where_nothing_is() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    assert_eq!(installer_link(&lib, &bin, "0.13.0"), "linked");
    assert_eq!(points_at(&bin.join("acs")), lib.join("0.13.0/acs"));
}

#[test]
fn the_installer_moves_its_link_forward() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    // 0.9.0 to 0.10.0: by number, not as text.
    symlink(&installed(&lib, "0.9.0"), &bin.join("acs"));
    assert_eq!(installer_link(&lib, &bin, "0.10.0"), "linked");
    assert_eq!(points_at(&bin.join("acs")), lib.join("0.10.0/acs"));
    // The same version again: still linked, which is a re-run's no-op.
    assert_eq!(installer_link(&lib, &bin, "0.10.0"), "linked");
    assert_eq!(points_at(&bin.join("acs")), lib.join("0.10.0/acs"));
}

#[test]
fn the_installer_never_moves_its_link_back() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    let newer = installed(&lib, "0.10.0");
    symlink(&newer, &bin.join("acs"));
    // ACS_VERSION=0.9.0: installed beside it, the link left on 0.10.0.
    assert_eq!(installer_link(&lib, &bin, "0.9.0"), "newer");
    assert_eq!(points_at(&bin.join("acs")), newer);
    assert!(lib.join("0.9.0/acs").is_file());
}

#[test]
fn the_installer_relinks_from_a_removed_version() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    let gone = installed(&lib, "0.14.0");
    symlink(&gone, &bin.join("acs"));
    std::fs::remove_dir_all(gone.parent().unwrap()).unwrap();
    assert_eq!(installer_link(&lib, &bin, "0.13.0"), "linked");
    assert_eq!(points_at(&bin.join("acs")), lib.join("0.13.0/acs"));
}

#[test]
fn the_installer_knows_its_link_through_a_symlinked_directory() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    installed(&lib, "0.12.0");
    // As a link made through a symlinked $HOME reads.
    symlink(&lib, &t.path().join("alias"));
    symlink(&t.path().join("alias/0.12.0/acs"), &bin.join("acs"));
    assert_eq!(installer_link(&lib, &bin, "0.13.0"), "linked");
    assert_eq!(points_at(&bin.join("acs")), lib.join("0.13.0/acs"));
}

#[test]
fn the_installer_leaves_a_users_own_file_alone() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("acs"), "mine\n").unwrap();
    assert_eq!(installer_link(&lib, &bin, "0.13.0"), "theirs");
    let meta = std::fs::symlink_metadata(bin.join("acs")).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(std::fs::read_to_string(bin.join("acs")).unwrap(), "mine\n");
}

#[test]
fn the_installer_leaves_a_foreign_link_alone() {
    let t = TempDir::new();
    let (lib, bin) = (t.path().join("lib"), t.path().join("bin"));
    std::fs::create_dir_all(&bin).unwrap();
    // An acs of the user's own, in a directory laid out like ours, and a
    // dangling one: neither is under lib, so neither is ours to move.
    let theirs = installed(&t.path().join("src/target"), "release");
    for target in [theirs, t.path().join("nowhere/0.1.0/acs")] {
        let _ = std::fs::remove_file(bin.join("acs"));
        symlink(&target, &bin.join("acs"));
        assert_eq!(installer_link(&lib, &bin, "0.13.0"), "theirs");
        assert_eq!(points_at(&bin.join("acs")), target);
    }
}

/// acs-sseq: `scripts/update-tap.sh` decides "older than the tap's" with
/// `sort`, and acs with `release::compare`. They must agree on every pair
/// of releases, or the tap could be moved backwards.
#[test]
fn the_tap_orders_releases_as_acs_does() {
    let script = include_str!("../scripts/update-tap.sh");
    let sort = script
        .lines()
        .find_map(|l| {
            let at = l.find("sort -t.")?;
            Some(l[at..].split(" | ").next().unwrap().to_string())
        })
        .expect("the sort in update-tap.sh");
    let versions = [
        "0.9.9", "0.10.0", "0.2.0", "1.0.0", "0.99.99", "0.17.0", "0.17.1", "2.0.10", "2.0.9",
        "10.0.0",
    ];
    for a in versions {
        for b in versions {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf '%s\\n%s\\n' {a} {b} | {sort} | tail -n 1"))
                .output()
                .unwrap();
            let newest = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let want = match acs::release::compare(a, b).unwrap() {
                std::cmp::Ordering::Less => b,
                _ => a,
            };
            assert_eq!(newest, want, "{sort} on {a}, {b}");
        }
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

/// A real git repository at `<dir>/notes` holding the release scripts
/// (`release-changes.sh`, `release-binaries.sh` and the `release-tags.sh`
/// both read), with one empty commit per subject in `history` (oldest
/// first), and a tag wherever an entry is a `vX.Y.Z` or `X.Y.Z` instead of a
/// subject. It is its own `origin`, for `release-binaries.sh`'s fetch.
fn notes_checkout(dir: &Path, history: &[&str]) -> PathBuf {
    let root = dir.join("notes");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    for s in [
        "release-changes.sh",
        "release-binaries.sh",
        "release-tags.sh",
    ] {
        let dest = root.join("scripts").join(s);
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("scripts")
                .join(s),
            &dest,
        )
        .unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
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
    git(&["remote", "add", "origin", root.to_str().unwrap()]);
    for h in history {
        let tag = acs::release::parse_version(h).is_some();
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
            "0.1.0",
            "feat(timing): name the greeting as its own phase",
            "chore(beads): close acs-ftn, file the phase-name drift",
            "fix(e2e): check the client binary",
            "chore(ci): pin the runner image",
            "chore(release): 0.2.0",
            "0.2.0",
        ],
    );
    assert_eq!(
        changes(&root, "0.2.0"),
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
            "0.1.0",
        ],
    );
    assert_eq!(
        changes(&root, "0.1.0"),
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
            "0.1.0",
            "chore(beads): close acs-ftn",
            "chore(release): 0.2.0",
            "0.2.0",
        ],
    );
    assert!(changes(&root, "0.2.0").is_empty());
}

/// The bridge (docs/VERSIONING.md, "Tags"): the releases up to v0.18.0 are
/// v-tagged and 0.19.0 on are not. The previous release is found by version
/// across both forms, so 0.19.0's notes list what landed since v0.18.0 —
/// not since some older v-tag, nor everything as a first release — and
/// v0.18.0's own notes still run from v0.17.0. The default is the newest.
#[test]
fn release_notes_find_the_previous_tag_across_both_forms() {
    let dir = TempDir::new();
    let root = notes_checkout(
        dir.path(),
        &[
            "feat: in 0.9",
            "v0.9.0",
            "feat: in 0.17",
            "v0.17.0",
            "fix: in 0.18",
            "chore(release): v0.18.0",
            "v0.18.0",
            "feat: in 0.19",
            "chore(release): 0.19.0",
            "0.19.0",
            "fix: in 0.19.1",
            "chore(release): 0.19.1",
            "0.19.1",
        ],
    );
    assert_eq!(changes(&root, "0.19.0"), ["- feat: in 0.19"]);
    assert_eq!(changes(&root, "v0.18.0"), ["- fix: in 0.18"]);
    assert_eq!(changes(&root, "0.19.1"), ["- fix: in 0.19.1"]);
    assert_eq!(changes(&root, ""), ["- fix: in 0.19.1"]);
}

/// `scripts/release-tags.sh`: every release tag, newest first by version,
/// both forms in one order — `git tag --sort=-v:refname` would put each
/// `v`-tag above every bare one, and `v0.18.0` above `0.19.0`.
#[test]
fn release_tags_order_both_forms_as_one_sequence() {
    let dir = TempDir::new();
    let root = notes_checkout(
        dir.path(),
        &[
            "a", "v0.2.0", "b", "v0.10.0", "c", "v0.18.0", "d", "0.19.0", "e", "0.19.10", "f",
            "0.19.2",
        ],
    );
    let out = Command::new("sh")
        .arg(root.join("scripts/release-tags.sh"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .collect::<Vec<_>>(),
        ["0.19.10", "0.19.2", "0.19.0", "v0.18.0", "v0.10.0", "v0.2.0"]
    );
}

/// What `scripts/release-binaries.sh` sets out to build: the tag it is given
/// in either form, or with none the newest release by version. It runs
/// against a `cargo` that fails, so it stops right after naming the tag
/// and checking it out. `v0.18.0` is published from a checkout that already
/// tags bare (the bridge), so it must still be taken by name.
#[test]
fn release_binaries_takes_either_tag_form_and_finds_the_latest() {
    let dir = TempDir::new();
    let root = notes_checkout(dir.path(), &["feat: a", "v0.17.0", "feat: b", "v0.18.0"]);
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh
exit 1
",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let building = |args: &[&str]| -> String {
        let out = Command::new("sh")
            .arg(root.join("scripts/release-binaries.sh"))
            .args(args)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(!out.status.success(), "the fake cargo should stop it");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("== building ").map(str::to_string))
            .unwrap_or_else(|| panic!("{out:?}"))
    };
    // Only v-tags yet: the newest is v0.18.0.
    assert_eq!(building(&[]), "v0.18.0");
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(st.success());
    };
    git(&["tag", "0.19.0"]);
    // A bare tag beside them: the newest by version, whatever its form.
    assert_eq!(building(&[]), "0.19.0");
    // Either form by name.
    assert_eq!(building(&["v0.18.0"]), "v0.18.0");
    assert_eq!(building(&["0.19.0"]), "0.19.0");
    // And no worktree is left behind by the failed build.
    let out = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
}
