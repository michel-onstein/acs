//! The release's Homebrew step: `cargo xtask formula` renders the formula and
//! `scripts/update-tap.sh` commits it to a tap — here a local bare repository
//! (`ACS_TAP_REPO`): new versions land, an unchanged or older one does not,
//! and --dry-run pushes nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch {
    root: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Scratch {
    fn new() -> Scratch {
        let root = std::env::temp_dir().join(format!("acs-tap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Git sees only what the test sets: no signing, no identity needed.
        std::fs::write(root.join("gitconfig"), "").unwrap();
        let s = Scratch { root };
        s.git(&s.root, &["init", "-q", "--bare", "-b", "main", "tap.git"]);
        let seed = s.root.join("seed");
        s.git(&s.root, &["clone", "-q", "tap.git", "seed"]);
        std::fs::write(seed.join("README.md"), "# homebrew-acs\n").unwrap();
        s.git(&seed, &["add", "README.md"]);
        s.git(&seed, &["commit", "-q", "-m", "tap"]);
        s.git(&seed, &["push", "-q", "origin", "HEAD:main"]);
        s
    }

    fn tap(&self) -> PathBuf {
        self.root.join("tap.git")
    }

    fn env(&self, c: &mut Command) {
        c.env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("ACS_TAP_REPO", self.tap());
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        self.env(&mut c);
        let out = c.arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `cargo xtask formula` for `version`, with made-up checksums.
    fn render(&self, version: &str) -> PathBuf {
        let sums: String = [
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-musl",
        ]
        .iter()
        .map(|t| format!("{}  acs-{version}-{t}.tar.gz\n", "a".repeat(64)))
        .collect();
        let sums_file = self.root.join(format!("SHA256SUMS-{version}"));
        std::fs::write(&sums_file, sums).unwrap();
        let formula = self.root.join(format!("acs-{version}.rb"));
        let st = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["formula", "--version", version, "--sums"])
            .arg(&sums_file)
            .arg("--out")
            .arg(&formula)
            .status()
            .unwrap();
        assert!(st.success());
        formula
    }

    /// `scripts/update-tap.sh <formula> <args>`.
    fn update(&self, formula: &Path, args: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/update-tap.sh");
        let mut c = Command::new("sh");
        self.env(&mut c);
        let out = c.arg(script).arg(formula).args(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    }

    fn log(&self) -> Vec<String> {
        self.git(&self.tap(), &["log", "--format=%s", "main"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn formula_on_tap(&self) -> String {
        self.git(&self.tap(), &["show", "main:Formula/acs.rb"])
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn a_release_updates_the_tap_once() {
    let s = Scratch::new();
    let f = s.render("1.9.0");

    // A dry run shows the change and pushes nothing.
    let out = s.update(&f, &["--dry-run"]);
    assert!(
        stdout(&out).contains("dry run: would commit acs 1.9.0"),
        "{out:?}"
    );
    assert_eq!(s.log(), ["tap"]);

    let out = s.update(&f, &[]);
    assert!(stdout(&out).contains("tap: acs 1.9.0 pushed"), "{out:?}");
    assert_eq!(s.log(), ["acs 1.9.0", "tap"]);
    let rendered = std::fs::read_to_string(&f).unwrap();
    assert_eq!(s.formula_on_tap(), rendered.trim_end());
    assert!(
        rendered.contains("/releases/download/v1.9.0/acs-1.9.0-x86_64-unknown-linux-musl.tar.gz")
    );

    // The same release again (re-published): nothing to commit.
    let out = s.update(&f, &[]);
    assert!(stdout(&out).contains("already acs 1.9.0"), "{out:?}");
    assert_eq!(s.log(), ["acs 1.9.0", "tap"]);

    // The next release replaces it.
    s.update(&s.render("1.10.0"), &[]);
    assert_eq!(s.log(), ["acs 1.10.0", "acs 1.9.0", "tap"]);
    assert!(s.formula_on_tap().contains("/v1.10.0/acs-1.10.0-"));

    // Re-publishing an older release does not move the tap back.
    let out = s.update(&f, &[]);
    assert!(
        stdout(&out).contains("newer than 1.9.0; left alone"),
        "{out:?}"
    );
    assert_eq!(s.log(), ["acs 1.10.0", "acs 1.9.0", "tap"]);
}
