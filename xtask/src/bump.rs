//! Automatic semantic versioning: `cargo xtask bump` (`scripts/version-bump.sh`).
//!
//! Reads every commit on the release branch since the last `vX.Y.Z` tag,
//! decides how far the version moves, and releases it: a `chore(release):
//! vX.Y.Z` commit (Cargo.toml and Cargo.lock) and an annotated tag, pushed
//! together to the remote from a throwaway worktree. The rules are in
//! docs/VERSIONING.md:
//!
//! - PATCH for a small fix; MINOR for a feature or a larger fix;
//! - MAJOR only when asked (`--major`), which resets MINOR and PATCH;
//! - docs, tests, beads, scripts and other non-shipping changes release
//!   nothing.

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How far a change moves the version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    None,
    Patch,
    Minor,
    Major,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "skip" => Some(Level::None),
            "patch" => Some(Level::Patch),
            "minor" => Some(Level::Minor),
            "major" => Some(Level::Major),
            _ => None,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::None => "none",
            Level::Patch => "patch",
            Level::Minor => "minor",
            Level::Major => "major",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// `1.2.3` or `v1.2.3`; nothing else (no pre-release suffixes).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        let mut it = s.split('.');
        let v = Version {
            major: it.next()?.parse().ok()?,
            minor: it.next()?.parse().ok()?,
            patch: it.next()?.parse().ok()?,
        };
        it.next().is_none().then_some(v)
    }

    pub fn bump(self, level: Level) -> Version {
        match level {
            Level::None => self,
            Level::Patch => Version {
                patch: self.patch + 1,
                ..self
            },
            Level::Minor => Version {
                minor: self.minor + 1,
                patch: 0,
                ..self
            },
            Level::Major => Version {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

/// One commit on the release branch.
#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub body: String,
    /// `(path, lines added + deleted)`; binary files count 0.
    pub files: Vec<(String, u64)>,
    /// `semver:*` label of the pull request the commit came from, if any.
    pub label: Option<Level>,
}

/// What one commit asks for, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub level: Level,
    pub reason: String,
}

fn decision(level: Level, reason: impl Into<String>) -> Decision {
    Decision {
        level,
        reason: reason.into(),
    }
}

/// Files that end up in the shipped binary (or change what it builds from).
pub fn is_shipping(path: &str) -> bool {
    path.starts_with("src/") || path == "build.rs" || path == "Cargo.toml" || path == "Cargo.lock"
}

/// `feat(scope)!: subject` → `("feat", true)`.
pub fn conventional(subject: &str) -> Option<(String, bool)> {
    let colon = subject.find(": ")?;
    let head = &subject[..colon];
    let (head, bang) = match head.strip_suffix('!') {
        Some(h) => (h, true),
        None => (head, false),
    };
    let kind = match head.find('(') {
        Some(i) if head.ends_with(')') => &head[..i],
        Some(_) => return None,
        None => head,
    };
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    Some((kind.to_string(), bang))
}

/// A `Semver: <level>` trailer in the commit body. `major` is not honoured
/// here: MAJOR only happens when asked for explicitly (`--major`).
pub fn trailer(body: &str) -> Option<Level> {
    body.lines().rev().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case("semver") {
            return None;
        }
        Level::parse(v)
    })
}

/// Decide one commit's level. `large` is the number of changed lines of
/// shipped code from which a fix counts as a larger fix (MINOR).
pub fn classify(c: &Commit, large: u64) -> Decision {
    if c.subject.starts_with("chore(release):") {
        return decision(Level::None, "release commit");
    }
    let explicit = trailer(&c.body)
        .map(|l| (l, "Semver trailer"))
        .or(c.label.map(|l| (l, "semver label")));
    if let Some((l, why)) = explicit {
        let l = if l == Level::Major { Level::Minor } else { l };
        return decision(l, format!("{why}: {l}"));
    }
    let shipped: u64 = c
        .files
        .iter()
        .filter(|(p, _)| is_shipping(p))
        .map(|(_, n)| n)
        .sum();
    let ships = c.files.iter().any(|(p, _)| is_shipping(p));
    let breaking = c.body.lines().any(|l| l.starts_with("BREAKING CHANGE"));
    let base = match conventional(&c.subject) {
        Some((kind, bang)) => {
            let breaking = bang || breaking;
            match kind.as_str() {
                "feat" => decision(Level::Minor, "feature"),
                _ if breaking => decision(Level::Minor, "breaking change (MAJOR only on request)"),
                "fix" | "perf" | "refactor" | "revert" => decision(Level::Patch, kind.clone()),
                "docs" | "chore" | "test" | "ci" | "style" | "build" => {
                    decision(Level::None, format!("{kind}: does not ship"))
                }
                other if ships => decision(Level::Patch, format!("{other}: changes shipped code")),
                other => decision(Level::None, format!("{other}: no shipped code")),
            }
        }
        None if breaking => decision(Level::Minor, "breaking change (MAJOR only on request)"),
        None if ships => decision(Level::Patch, "changes shipped code"),
        None => decision(Level::None, "no shipped code changed"),
    };
    if base.level == Level::Patch && shipped >= large {
        return decision(
            Level::Minor,
            format!("larger fix: {shipped} lines of shipped code (≥ {large})"),
        );
    }
    base
}

/// The highest level any commit asks for.
pub fn aggregate(decisions: &[Decision]) -> Level {
    decisions
        .iter()
        .map(|d| d.level)
        .max()
        .unwrap_or(Level::None)
}

/// Replace the `[package]` version in a Cargo.toml.
pub fn set_package_version(toml: &str, v: Version) -> Option<String> {
    let mut out = String::with_capacity(toml.len());
    let mut in_package = false;
    let mut done = false;
    for line in toml.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
        }
        if in_package && !done && t.starts_with("version") && t.contains('=') {
            let nl = if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!("version = \"{v}\"{nl}"));
            done = true;
        } else {
            out.push_str(line);
        }
    }
    done.then_some(out)
}

/// The `[package]` version of a Cargo.toml.
pub fn package_version(toml: &str) -> Option<Version> {
    let mut in_package = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
        } else if in_package && t.starts_with("version") {
            let v = t.split_once('=')?.1.trim().trim_matches('"');
            return Version::parse(v);
        }
    }
    None
}

// ---- driving git -------------------------------------------------------------

struct Opts {
    repo: PathBuf,
    remote: String,
    branch: String,
    dry_run: bool,
    force: Option<Level>,
    large: u64,
    labels: bool,
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn last_tag(repo: &Path, rev: &str) -> Result<Option<(String, Version)>, String> {
    let tags = git(repo, &["tag", "--list", "v*", "--merged", rev])?;
    Ok(tags
        .lines()
        .filter_map(|t| Version::parse(t).map(|v| (t.to_string(), v)))
        .max_by_key(|(_, v)| *v))
}

fn pr_label(repo: &Path, pr: &str) -> Option<Level> {
    let out = Command::new("gh")
        .args(["pr", "view", pr, "--json", "labels", "-q", ".labels[].name"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("semver:").and_then(Level::parse))
}

fn commits(o: &Opts, range: &str) -> Result<Vec<Commit>, String> {
    let shas = git(&o.repo, &["rev-list", "--first-parent", "--reverse", range])?;
    let mut out = Vec::new();
    for sha in shas.lines().filter(|l| !l.is_empty()) {
        let msg = git(&o.repo, &["show", "-s", "--format=%s%x00%b", sha])?;
        let (subject, body) = msg.split_once('\0').unwrap_or((&msg, ""));
        let stat = git(
            &o.repo,
            &["show", "--numstat", "--format=", "--first-parent", sha],
        )?;
        let files = stat
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, '\t');
                let a = it.next()?.parse::<u64>().unwrap_or(0);
                let d = it.next()?.parse::<u64>().unwrap_or(0);
                Some((it.next()?.to_string(), a + d))
            })
            .collect();
        let label = if o.labels {
            subject
                .rsplit_once("(#")
                .and_then(|(_, r)| r.strip_suffix(')'))
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|n| pr_label(&o.repo, n))
        } else {
            None
        };
        out.push(Commit {
            sha: sha.to_string(),
            subject: subject.trim().to_string(),
            body: body.to_string(),
            files,
            label,
        });
    }
    Ok(out)
}

fn run(o: &Opts) -> Result<(), String> {
    git(&o.repo, &["fetch", "--tags", &o.remote, &o.branch])?;
    let head = format!("{}/{}", o.remote, o.branch);
    let toml = git(&o.repo, &["show", &format!("{head}:Cargo.toml")])?;
    let current = package_version(&toml).ok_or("no [package] version in Cargo.toml")?;

    let (range, base) = match last_tag(&o.repo, &head)? {
        Some((tag, v)) => (format!("{tag}..{head}"), v.max(current)),
        None => {
            // First release: the current version is released as it is.
            println!("no release tag yet: tagging {head} as v{current}");
            let new = o.force.map_or(current, |l| current.bump(l));
            return release(o, &head, new, &[], new != current);
        }
    };
    let list = commits(o, &range)?;
    let decisions: Vec<(Commit, Decision)> = list
        .into_iter()
        .map(|c| {
            let d = classify(&c, o.large);
            (c, d)
        })
        .collect();
    for (c, d) in &decisions {
        println!(
            "  {:<6} {}  {} — {}",
            d.level,
            &c.sha[..8],
            c.subject,
            d.reason
        );
    }
    let level = match o.force {
        Some(l) => l,
        None => aggregate(&decisions.iter().map(|(_, d)| d.clone()).collect::<Vec<_>>()),
    };
    if level == Level::None {
        println!("nothing to release since v{base} (current {current})");
        return Ok(());
    }
    let new = base.bump(level);
    println!("release: {base} → {new} ({level})");
    release(o, &head, new, &decisions, true)
}

fn release(
    o: &Opts,
    head: &str,
    new: Version,
    decisions: &[(Commit, Decision)],
    edit: bool,
) -> Result<(), String> {
    let tag = format!("v{new}");
    if o.dry_run {
        println!(
            "dry run: would {}tag {tag} and push to {}/{}",
            if edit { "commit, " } else { "" },
            o.remote,
            o.branch
        );
        return Ok(());
    }
    // Unguessable: /tmp is shared, and a name anyone can predict can be
    // planted ahead of us (acs-721).
    let tmp = std::env::temp_dir().join(format!("acs-release-{new}-{}", random_tag()));
    let tmp_s = tmp.to_string_lossy().into_owned();
    git(&o.repo, &["worktree", "add", "--detach", &tmp_s, head])?;
    let result = (|| -> Result<(), String> {
        if edit {
            let path = tmp.join("Cargo.toml");
            let toml = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let toml =
                set_package_version(&toml, new).ok_or("cannot set the version in Cargo.toml")?;
            std::fs::write(&path, toml).map_err(|e| e.to_string())?;
            let st = Command::new("cargo")
                .args(["update", "--workspace", "--offline", "--quiet"])
                .current_dir(&tmp)
                .status()
                .map_err(|e| format!("cargo: {e}"))?;
            if !st.success() {
                return Err("cargo update --workspace failed".into());
            }
            let mut msg = format!("chore(release): {tag}\n\n");
            for (c, d) in decisions {
                if d.level != Level::None {
                    msg.push_str(&format!("- {} {} ({})\n", d.level, c.subject, d.reason));
                }
            }
            let mut files = vec!["Cargo.toml"];
            if tmp.join("Cargo.lock").exists() {
                files.push("Cargo.lock");
            }
            let mut add = vec!["add", "--"];
            add.extend(files);
            git(&tmp, &add)?;
            git(&tmp, &["commit", "-q", "-m", &msg])?;
        }
        git(&tmp, &["tag", "-a", &tag, "-m", &format!("acs {new}")])?;
        let target = format!("HEAD:refs/heads/{}", o.branch);
        git(
            &tmp,
            &[
                "push",
                "--atomic",
                &o.remote,
                &target,
                &format!("refs/tags/{tag}"),
            ],
        )
        .map_err(|e| format!("{e}\n(the branch moved? fetch and run again)"))?;
        Ok(())
    })();
    let _ = git(&o.repo, &["worktree", "remove", "--force", &tmp_s]);
    if result.is_err() {
        let _ = git(&o.repo, &["tag", "-d", &tag]);
    }
    result?;
    println!(
        "released {tag} on {}/{}; update local {} with: git pull --ff-only",
        o.remote, o.branch, o.branch
    );
    Ok(())
}

pub fn main(args: &[String]) -> Result<(), String> {
    let mut o = Opts {
        repo: std::env::current_dir().map_err(|e| e.to_string())?,
        remote: "origin".into(),
        branch: "main".into(),
        dry_run: false,
        force: None,
        large: 300,
        labels: true,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--dry-run" | "-n" => o.dry_run = true,
            "--major" => o.force = Some(Level::Major),
            "--minor" => o.force = Some(Level::Minor),
            "--patch" => o.force = Some(Level::Patch),
            "--level" => o.force = Some(Level::parse(&val()?).ok_or("--level major|minor|patch")?),
            "--large-lines" => o.large = val()?.parse().map_err(|_| "--large-lines N")?,
            "--repo" => o.repo = PathBuf::from(val()?),
            "--remote" => o.remote = val()?,
            "--branch" => o.branch = val()?,
            "--no-labels" => o.labels = false,
            "-h" | "--help" => {
                println!("usage: cargo xtask bump [--dry-run] [--major|--minor|--patch] [--large-lines N]\n       [--repo DIR] [--remote NAME] [--branch NAME] [--no-labels]");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if o.force == Some(Level::None) {
        return Err("--level none makes no release".into());
    }
    run(&o)
}

/// Sixteen hex characters from `/dev/urandom`, for a temporary name in the
/// shared `/tmp` (acs-721). Falls back to the clock if it cannot be read.
fn random_tag() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
    {
        return b.iter().map(|x| format!("{x:02x}")).collect();
    }
    format!(
        "{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str, files: &[(&str, u64)]) -> Commit {
        Commit {
            sha: "0123456789abcdef".into(),
            subject: subject.into(),
            body: body.into(),
            files: files.iter().map(|(p, n)| (p.to_string(), *n)).collect(),
            label: None,
        }
    }

    #[test]
    fn versions() {
        let v = Version::parse("v1.4.7").unwrap();
        assert_eq!(v.bump(Level::Patch).to_string(), "1.4.8");
        assert_eq!(v.bump(Level::Minor).to_string(), "1.5.0");
        assert_eq!(v.bump(Level::Major).to_string(), "2.0.0");
        assert_eq!(v.bump(Level::None), v);
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3-rc1").is_none());
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
    }

    #[test]
    fn conventional_subjects() {
        assert_eq!(conventional("feat: x"), Some(("feat".into(), false)));
        assert_eq!(conventional("fix(proxy)!: x"), Some(("fix".into(), true)));
        assert_eq!(conventional("Fix master stall when x"), None);
        assert_eq!(conventional("acs v1: persistent ssh sessions"), None);
        assert_eq!(
            conventional("chore(release): v1.0.0"),
            Some(("chore".into(), false))
        );
    }

    #[test]
    fn classification() {
        let src = [("src/master.rs", 12)];
        let big = [("src/master.rs", 250), ("src/proxy.rs", 80)];
        let docs = [("docs/DESIGN.md", 40), ("README.md", 3)];
        let cases: Vec<(Commit, Level)> = vec![
            (commit("feat: add --new", "", &src), Level::Minor),
            (commit("fix: off-by-one in ring", "", &src), Level::Patch),
            (commit("fix: rework resume", "", &big), Level::Minor),
            (commit("refactor: split client", "", &src), Level::Patch),
            (commit("docs: explain resume", "", &docs), Level::None),
            (
                commit("test: more cases", "", &[("tests/x.rs", 500)]),
                Level::None,
            ),
            (
                commit("chore: bump deps", "", &[("Cargo.lock", 10)]),
                Level::None,
            ),
            (commit("feat!: new protocol", "", &src), Level::Minor),
            (commit("fix!: drop old flag", "", &src), Level::Minor),
            (commit("Fix master stall (#3)", "", &src), Level::Patch),
            (
                commit("Stop test sessions outliving their tests", "", &big),
                Level::Minor,
            ),
            (commit("Tidy the README", "", &docs), Level::None),
            (commit("fix: typo", "Semver: none", &src), Level::None),
            (
                commit("fix: subtle but important", "Semver: minor", &src),
                Level::Minor,
            ),
            (commit("fix: x", "Semver: major", &src), Level::Minor),
            (
                commit("chore(release): v0.2.0", "", &[("Cargo.toml", 2)]),
                Level::None,
            ),
            (
                commit("Rework the thing", "BREAKING CHANGE: flags renamed", &src),
                Level::Minor,
            ),
        ];
        for (c, want) in cases {
            let d = classify(&c, 300);
            assert_eq!(d.level, want, "{}: {}", c.subject, d.reason);
        }
        let mut labelled = commit("fix: tiny (#9)", "", &src);
        labelled.label = Some(Level::Minor);
        assert_eq!(classify(&labelled, 300).level, Level::Minor);
    }

    #[test]
    fn highest_level_wins() {
        let d = |l| decision(l, "");
        assert_eq!(aggregate(&[]), Level::None);
        assert_eq!(aggregate(&[d(Level::Patch), d(Level::None)]), Level::Patch);
        assert_eq!(
            aggregate(&[d(Level::Patch), d(Level::Minor), d(Level::None)]),
            Level::Minor
        );
    }

    #[test]
    fn cargo_toml_version() {
        let toml = "[package]\nname = \"acs\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibc = { version = \"0.2\" }\n\n[workspace]\n";
        assert_eq!(package_version(toml), Version::parse("0.1.0"));
        let out = set_package_version(toml, Version::parse("0.2.0").unwrap()).unwrap();
        assert!(out.contains("version = \"0.2.0\""));
        assert!(out.contains("libc = { version = \"0.2\" }"));
        assert_eq!(package_version(&out), Version::parse("0.2.0"));
        assert!(
            set_package_version("[dependencies]\n", Version::parse("1.0.0").unwrap()).is_none()
        );
    }
}
