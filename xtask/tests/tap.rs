//! The release's Homebrew step: `cargo xtask formula` renders the formula and
//! `scripts/update-tap.sh` commits it to a tap — here a local bare repository
//! (`ACS_TAP_REPO`): new versions land, an unchanged or older one does not,
//! and --dry-run pushes nothing.

use std::os::unix::fs::PermissionsExt;
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
    /// A fresh tap under a directory of its own: `name` keeps the tests,
    /// which run in parallel in one process, out of each other's.
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("acs-tap-test-{name}-{}", std::process::id()));
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
        let sums_file = self.sums(version);
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

    /// `scripts/update-tap.sh <formula> <args>`, which must succeed.
    fn update(&self, formula: &Path, args: &[&str]) -> Output {
        let out = self.try_update(formula, args);
        assert!(out.status.success(), "{out:?}");
        out
    }

    /// `scripts/update-tap.sh <formula> <args>`, however it ends.
    fn try_update(&self, formula: &Path, args: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/update-tap.sh");
        let mut c = Command::new("sh");
        self.env(&mut c);
        c.arg(script).arg(formula).args(args).output().unwrap()
    }

    /// A throwaway signing key under the scratch directory: its private
    /// half's path, and its public line.
    fn keygen(&self, name: &str) -> (PathBuf, String) {
        let key = self.root.join(name);
        let st = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(st.success());
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        (key, public.trim().to_string())
    }

    /// Publish the release tagged `tag` (`X.Y.Z`, or `vX.Y.Z` up to
    /// v0.18.0) under `releases/download/<tag>/` with made-up checksums, and
    /// with a `SHA256SUMS.sig` when `signer` names a key.
    fn publish(&self, tag: &str, signer: Option<&Path>) -> PathBuf {
        let version = tag.trim_start_matches('v');
        let dir = self.root.join(format!("releases/download/{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("SHA256SUMS.sig"));
        let sums = dir.join("SHA256SUMS");
        std::fs::copy(self.sums(version), &sums).unwrap();
        if let Some(key) = signer {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-Y", "sign", "-n", "acs-release", "-f"])
                .arg(key)
                .arg(&sums)
                .status()
                .unwrap();
            assert!(st.success());
        }
        sums
    }

    /// `scripts/update-tap.sh <tag> <args>` against the releases
    /// [`Scratch::publish`] writes, with an xtask whose build channel trusts
    /// `trusted` — the stand-in for a build made with that
    /// `ACS_DEFAULT_RELEASE_KEY`, which a test cannot recompile xtask for.
    /// Every xtask subcommand run is appended to `xtask.log`.
    fn try_update_release(&self, tag: &str, trusted: &str, args: &[&str]) -> Output {
        let wrapper = self.root.join("xtask.sh");
        let log = self.root.join("xtask.log");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{}'\nif [ \"$1\" = channel ]; then\n    printf 'releases %s\\nkey %s\\n' 'https://example.com/unused' '{trusted}'\n    exit 0\nfi\nexec '{}' \"$@\"\n",
                log.display(),
                env!("CARGO_BIN_EXE_xtask"),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/update-tap.sh");
        let mut c = Command::new("sh");
        self.env(&mut c);
        c.env("ACS_XTASK", &wrapper)
            .env(
                "ACS_RELEASES_URL",
                format!("file://{}", self.root.join("releases").display()),
            )
            .arg(script)
            .arg(tag)
            .args(args)
            .output()
            .unwrap()
    }

    /// The xtask subcommands [`Scratch::try_update_release`] ran.
    fn xtask_runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("xtask.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A `SHA256SUMS` for `version`, made-up checksums.
    fn sums(&self, version: &str) -> PathBuf {
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
        sums_file
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
    let s = Scratch::new("once");
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
        rendered.contains("/releases/download/1.9.0/acs-1.9.0-x86_64-unknown-linux-musl.tar.gz")
    );

    // The same release again (re-published): nothing to commit.
    let out = s.update(&f, &[]);
    assert!(stdout(&out).contains("already acs 1.9.0"), "{out:?}");
    assert_eq!(s.log(), ["acs 1.9.0", "tap"]);

    // The next release replaces it.
    s.update(&s.render("1.10.0"), &[]);
    assert_eq!(s.log(), ["acs 1.10.0", "acs 1.9.0", "tap"]);
    assert!(s.formula_on_tap().contains("/1.10.0/acs-1.10.0-"));

    // Re-publishing an older release does not move the tap back.
    let out = s.update(&f, &[]);
    assert!(
        stdout(&out).contains("newer than 1.9.0; left alone"),
        "{out:?}"
    );
    assert_eq!(s.log(), ["acs 1.10.0", "acs 1.9.0", "tap"]);
}

/// acs-u3qn: a fork whose releases are not GitHub-shaped — no
/// `/releases/download/` in its URLs — renders a valid formula, and the tap
/// script takes its version from the line the generator writes for it
/// rather than rejecting it as naming no release.
#[test]
fn a_formula_whose_urls_are_not_github_shaped_still_lands() {
    let s = Scratch::new("fork");
    let f = s.render("2.0.0");
    let text = std::fs::read_to_string(&f).unwrap();
    let upstream = "https://github.com/michel-onstein/acs/releases/download/";
    assert!(text.contains(upstream), "{text}");
    let fork = text.replace(upstream, "https://dl.example.com/acs/2/");
    assert!(!fork.contains("/releases/download/"), "{fork}");
    let fork_file = s.root.join("fork.rb");
    std::fs::write(&fork_file, &fork).unwrap();

    let out = s.update(&fork_file, &[]);
    assert!(stdout(&out).contains("tap: acs 2.0.0 pushed"), "{out:?}");
    assert_eq!(s.log(), ["acs 2.0.0", "tap"]);

    // And the version on the tap is read the same way: an older one is
    // still left alone.
    let out = s.update(&s.render("1.9.0"), &[]);
    assert!(
        stdout(&out).contains("already at acs 2.0.0, newer than 1.9.0"),
        "{out:?}"
    );

    // A formula that does not say which release it is is refused, URLs or
    // no URLs: nothing reads them for that any more.
    let unmarked: String = text
        .lines()
        .filter(|l| !l.starts_with("# acs-version: "))
        .map(|l| format!("{l}\n"))
        .collect();
    let unmarked_file = s.root.join("unmarked.rb");
    std::fs::write(&unmarked_file, unmarked).unwrap();
    let out = s.try_update(&unmarked_file, &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("names no release"),
        "{out:?}"
    );
    assert_eq!(s.log(), ["acs 2.0.0", "tap"]);
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// acs-oxl4: `scripts/update-tap.sh vX.Y.Z` renders a published release's
/// formula only once that release's SHA256SUMS.sig verifies against the
/// build's release key. Unsigned, signed by another key, or altered after
/// signing, it is refused before xtask renders anything and the tap does
/// not move; signed by the build's key, it lands.
#[test]
fn a_published_release_lands_only_when_signed_by_the_build_key() {
    let s = Scratch::new("signed");
    let (release_key, trusted) = s.keygen("release");
    let (other_key, _) = s.keygen("other");

    // No signature at all.
    s.publish("1.9.0", None);
    let out = s.try_update_release("1.9.0", &trusted, &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains("has no signature for SHA256SUMS"),
        "{out:?}"
    );
    assert_eq!(s.xtask_runs(), ["channel"], "rendered before refusing");
    assert_eq!(s.log(), ["tap"]);

    // Signed, but by a key the build does not trust.
    s.publish("1.9.0", Some(&other_key));
    let out = s.try_update_release("1.9.0", &trusted, &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains("is not signed by this build's release key"),
        "{out:?}"
    );
    assert_eq!(
        s.xtask_runs(),
        ["channel", "channel"],
        "rendered before refusing"
    );
    assert_eq!(s.log(), ["tap"]);

    // Signed by the right key, then altered: the signature no longer covers
    // what would be rendered.
    let sums = s.publish("1.9.0", Some(&release_key));
    let tampered =
        std::fs::read_to_string(&sums)
            .unwrap()
            .replacen(&"a".repeat(64), &"b".repeat(64), 1);
    std::fs::write(&sums, tampered).unwrap();
    let out = s.try_update_release("1.9.0", &trusted, &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains("is not signed by this build's release key"),
        "{out:?}"
    );
    assert!(
        !s.xtask_runs().iter().any(|r| r == "formula"),
        "{:?}",
        s.xtask_runs()
    );
    assert_eq!(s.log(), ["tap"]);

    // Signed by the build's key: rendered and pushed.
    s.publish("1.9.0", Some(&release_key));
    let out = s.try_update_release("1.9.0", &trusted, &[]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("tap: acs 1.9.0 pushed"), "{out:?}");
    assert_eq!(s.xtask_runs().last().map(String::as_str), Some("formula"));
    assert_eq!(s.log(), ["acs 1.9.0", "tap"]);
    assert!(s.formula_on_tap().contains("# acs-version: 1.9.0"));
}

/// The tag cutoff (docs/VERSIONING.md, "Tags"): `v0.18.0` was published with
/// its `v`, from a checkout that already tags bare; retrying its tap update
/// by that tag still finds its SHA256SUMS and renders a formula that
/// downloads from `v0.18.0`.
#[test]
fn a_v_tagged_release_still_lands_by_its_tag() {
    let s = Scratch::new("vtag");
    let (release_key, trusted) = s.keygen("release");
    s.publish("v0.18.0", Some(&release_key));
    let out = s.try_update_release("v0.18.0", &trusted, &[]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("tap: acs 0.18.0 pushed"), "{out:?}");
    let f = s.formula_on_tap();
    assert!(f.contains("# acs-version: 0.18.0"), "{f}");
    assert!(f.contains("/releases/download/v0.18.0/acs-0.18.0-"), "{f}");
}

/// The key `scripts/update-tap.sh` trusts is the one xtask was built with —
/// the same constant the formula and the packaged installer carry — so a
/// fork built with its own `ACS_DEFAULT_RELEASE_KEY` checks its own
/// releases with no edit to the script (acs-oxl4).
#[test]
fn xtask_channel_names_the_build_release_key() {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("channel")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    let key = format!("key {}", acs::signature::RELEASE_KEY);
    let releases = format!("releases {}", acs::release::DEFAULT_RELEASES_URL);
    assert!(text.lines().any(|l| l == key), "{text}");
    assert!(text.lines().any(|l| l == releases), "{text}");
}
