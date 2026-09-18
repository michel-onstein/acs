//! `cargo xtask bump` end to end against a scratch repository with a bare
//! remote: tags, release commits, Cargo.toml/Cargo.lock, the level rules,
//! re-runs and --major.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Scratch {
    root: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Scratch {
    fn work(&self) -> PathBuf {
        self.root.join("w")
    }
    fn remote(&self) -> PathBuf {
        self.root.join("r.git")
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn setup() -> Scratch {
    let root = std::env::temp_dir().join(format!("acs-bump-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let s = Scratch { root };
    git(
        &s.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            s.remote().to_str().unwrap(),
        ],
    );
    git(
        &s.root,
        &["init", "-q", "-b", "main", s.work().to_str().unwrap()],
    );
    let w = s.work();
    git(&w, &["config", "user.name", "Test"]);
    git(&w, &["config", "user.email", "test@example.com"]);
    git(&w, &["config", "commit.gpgsign", "false"]);
    git(&w, &["config", "tag.gpgsign", "false"]);
    git(
        &w,
        &["remote", "add", "origin", s.remote().to_str().unwrap()],
    );
    std::fs::create_dir_all(w.join("src")).unwrap();
    std::fs::write(
        w.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(w.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let st = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--quiet"])
        .current_dir(&w)
        .status()
        .unwrap();
    assert!(st.success());
    git(&w, &["add", "."]);
    git(&w, &["commit", "-q", "-m", "Initial"]);
    git(&w, &["push", "-q", "origin", "main"]);
    s
}

/// Make a commit on top of the remote's main and push it.
fn change(s: &Scratch, subject: &str, file: &str, lines: usize) {
    let w = s.work();
    git(&w, &["pull", "-q", "--ff-only", "origin", "main"]);
    let path = w.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    for i in 0..lines {
        text.push_str(&format!("// {subject} {i}\n"));
    }
    std::fs::write(&path, text).unwrap();
    git(&w, &["add", "."]);
    git(&w, &["commit", "-q", "-m", subject]);
    git(&w, &["push", "-q", "origin", "main"]);
}

fn bump(s: &Scratch, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["bump", "--no-labels", "--repo"])
        .arg(s.work())
        .args(extra)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    text
}

fn remote_tags(s: &Scratch) -> Vec<String> {
    let t = git(&s.remote(), &["tag", "--list", "v*", "--sort=v:refname"]);
    t.lines().map(str::to_string).collect()
}

fn remote_file(s: &Scratch, file: &str) -> String {
    git(&s.remote(), &["show", &format!("main:{file}")])
}

fn remote_version(s: &Scratch) -> String {
    let toml = remote_file(s, "Cargo.toml");
    toml.lines()
        .find(|l| l.starts_with("version"))
        .unwrap()
        .split('"')
        .nth(1)
        .unwrap()
        .to_string()
}

#[test]
fn release_lifecycle() {
    let s = setup();

    // First run: the current version becomes the first release.
    bump(&s, &[]);
    assert_eq!(remote_tags(&s), ["v0.1.0"]);
    assert_eq!(remote_version(&s), "0.1.0");

    // A small fix: PATCH, with Cargo.toml and Cargo.lock updated.
    change(&s, "fix: off by one", "src/lib.rs", 2);
    let out = bump(&s, &[]);
    assert!(out.contains("0.1.0 → 0.1.1 (patch)"), "{out}");
    assert_eq!(remote_tags(&s), ["v0.1.0", "v0.1.1"]);
    assert_eq!(remote_version(&s), "0.1.1");
    assert!(remote_file(&s, "Cargo.lock").contains("version = \"0.1.1\""));
    let subject = git(&s.remote(), &["log", "-1", "--format=%s", "main"]);
    assert_eq!(subject, "chore(release): v0.1.1");
    assert_eq!(
        git(&s.remote(), &["rev-list", "-n1", "v0.1.1"]),
        git(&s.remote(), &["rev-parse", "main"]),
        "the tag points at the release commit"
    );

    // Re-running releases nothing.
    let out = bump(&s, &[]);
    assert!(out.contains("nothing to release"), "{out}");
    assert_eq!(remote_tags(&s).len(), 2);

    // Docs only: nothing to release.
    change(&s, "docs: explain", "README.md", 20);
    let out = bump(&s, &[]);
    assert!(out.contains("nothing to release"), "{out}");

    // A large fix without a conventional prefix: MINOR.
    change(&s, "Rework the parser", "src/lib.rs", 400);
    let out = bump(&s, &[]);
    assert!(out.contains("0.1.1 → 0.2.0 (minor)"), "{out}");
    assert!(out.contains("larger fix"), "{out}");

    // A dry run changes nothing; a feature is MINOR.
    change(&s, "feat: new thing", "src/lib.rs", 3);
    let out = bump(&s, &["--dry-run"]);
    assert!(out.contains("0.2.0 → 0.3.0 (minor)"), "{out}");
    assert_eq!(remote_tags(&s).last().unwrap(), "v0.2.0");
    bump(&s, &[]);
    assert_eq!(remote_version(&s), "0.3.0");

    // MAJOR only on request, resetting MINOR and PATCH.
    change(&s, "fix: small", "src/lib.rs", 1);
    let out = bump(&s, &["--major"]);
    assert!(out.contains("0.3.0 → 1.0.0 (major)"), "{out}");
    assert_eq!(remote_tags(&s).last().unwrap(), "v1.0.0");
    assert_eq!(remote_version(&s), "1.0.0");

    // No throwaway worktree is left behind.
    let list = git(&s.work(), &["worktree", "list"]);
    assert_eq!(list.lines().count(), 1, "{list}");
}
