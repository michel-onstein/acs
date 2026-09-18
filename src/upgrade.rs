//! `acs upgrade` (DESIGN §7.5): replace this acs with the latest (or a
//! given) GitHub release.
//!
//! The download is checked against the release's `SHA256SUMS`, run once
//! (`--version`) to prove it works here, and only then renamed into place,
//! so the old binary stays until the new one is known good. Two layouts:
//!
//! - **a plain file** (a manual install, `ACS_INSTALL_DIR`): replaced in
//!   place, keeping its mode;
//! - **versioned** (`…/acs/<version>/acs`, as the installer and the remote
//!   install lay it out, DESIGN §8): the new version goes next to it and the
//!   links that pointed at this one (`~/.local/bin/acs`, `/usr/local/bin/acs`,
//!   the one on `PATH`) are repointed; the old version stays for clients that
//!   still use it on this host, and is pruned like any other.
//!
//! Remote hosts need nothing: the next connection finds no binary of the new
//! version there and installs it (DESIGN §8).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::release::{self, Asset};

pub const USAGE: &str = "\
usage: acs upgrade [--version X.Y.Z] [--check]

Replace this acs with the latest release from GitHub (checked against the
release's SHA256SUMS). Remote hosts get the new version on the next connect.

  --version X.Y.Z  install that release, even an older one
  --check          only say whether a newer release exists";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Opts {
    pub version: Option<String>,
    pub check: bool,
    pub help: bool,
}

pub fn parse(args: &[OsString]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--version" | "-V" => o.version = Some(it.next().ok_or("--version needs X.Y.Z")?),
            s if s.starts_with("--version=") => o.version = Some(s[10..].to_string()),
            "-h" | "--help" => o.help = true,
            other => {
                return Err(format!(
                    "unexpected argument {other} (see acs upgrade --help)"
                ))
            }
        }
    }
    if let Some(v) = &o.version {
        if release::parse_version(v).is_none() {
            return Err(format!("--version wants X.Y.Z, not '{v}'"));
        }
    }
    Ok(o)
}

/// What to do given our version and the release found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Install,
    /// The release is the version we are.
    Current,
    /// We are newer than the latest release (and no version was asked for).
    Newer,
}

pub fn decide(current: &str, available: &str, pinned: bool) -> Decision {
    use std::cmp::Ordering::*;
    match release::compare(available, current) {
        Some(Equal) => Decision::Current,
        Some(Greater) => Decision::Install,
        Some(Less) if pinned => Decision::Install,
        Some(Less) => Decision::Newer,
        None if pinned => Decision::Install,
        None => Decision::Newer,
    }
}

/// Where the running binary is and how to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// Replace this file.
    Plain(PathBuf),
    /// `root/<version>/acs`: install `root/<new>/acs` and repoint `links`.
    Versioned { root: PathBuf, links: Vec<PathBuf> },
}

impl Layout {
    /// `exe` is the running binary's real path; `candidates` are paths that
    /// may be links to it.
    pub fn detect(exe: &Path, version: &str, candidates: &[PathBuf]) -> Layout {
        let dir = exe.parent();
        let versioned = exe.file_name().is_some_and(|n| n == "acs")
            && dir
                .and_then(Path::file_name)
                .is_some_and(|n| n == std::ffi::OsStr::new(version));
        match dir.and_then(Path::parent) {
            Some(root) if versioned => {
                let mut links: Vec<PathBuf> = Vec::new();
                for c in candidates {
                    let is_link =
                        std::fs::symlink_metadata(c).is_ok_and(|m| m.file_type().is_symlink());
                    if is_link
                        && std::fs::canonicalize(c).is_ok_and(|t| t == exe)
                        && !links.contains(c)
                    {
                        links.push(c.clone());
                    }
                }
                Layout::Versioned {
                    root: root.to_path_buf(),
                    links,
                }
            }
            _ => Layout::Plain(exe.to_path_buf()),
        }
    }

    /// Where the new version's binary goes.
    pub fn destination(&self, version: &str) -> PathBuf {
        match self {
            Layout::Plain(p) => p.clone(),
            Layout::Versioned { root, .. } => root.join(version).join("acs"),
        }
    }

    /// Every directory the upgrade writes to.
    fn dirs(&self, version: &str) -> Vec<PathBuf> {
        let mut v = Vec::new();
        match self {
            Layout::Plain(p) => v.extend(p.parent().map(Path::to_path_buf)),
            Layout::Versioned { root, links } => {
                let dest = root.join(version);
                v.push(if dest.exists() { dest } else { root.clone() });
                v.extend(
                    links
                        .iter()
                        .filter_map(|l| l.parent().map(Path::to_path_buf)),
                );
            }
        }
        v
    }
}

/// Paths that may be links to the running binary: what it was run as, the
/// usual bin directories, and `acs` on `PATH`.
fn link_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(a0) = std::env::args_os().next().map(PathBuf::from) {
        if a0.components().count() > 1 {
            v.push(std::path::absolute(&a0).unwrap_or(a0));
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        v.push(PathBuf::from(home).join(".local/bin/acs"));
    }
    v.push(PathBuf::from("/usr/local/bin/acs"));
    if let Some(path) = std::env::var_os("PATH") {
        v.extend(std::env::split_paths(&path).map(|d| d.join("acs")));
    }
    v
}

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Failed(String),
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::Failed(s)
    }
}

pub fn main(args: &[OsString]) -> ExitCode {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&opts) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(e)) => {
            eprintln!("acs: {e}");
            ExitCode::from(2)
        }
        Err(Error::Failed(e)) => {
            eprintln!("acs: {e}");
            ExitCode::from(1)
        }
    }
}

/// A temporary directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        let p = std::env::temp_dir().join(format!(
            "acs-upgrade.{}.{:08x}",
            crate::sys::getpid(),
            crate::sys::random_u64() as u32
        ));
        std::fs::create_dir(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(Scratch(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(opts: &Opts) -> Result<String, Error> {
    let current = crate::VERSION;
    let target = release::archive_target(crate::payload::OWN_TARGET);
    let base = release::releases_url();
    let tmp = Scratch::new()?;
    let asset = release::lookup(&base, opts.version.as_deref(), &target, &tmp.0, 30)?;
    let v = asset.version.clone();
    match decide(current, &v, opts.version.is_some()) {
        Decision::Current => {
            return Ok(if opts.version.is_some() {
                format!("acs {v} is already installed")
            } else {
                format!("acs {v} is the latest release; nothing to do")
            })
        }
        Decision::Newer => {
            return Ok(format!(
                "acs {current} is newer than the latest release ({v}); nothing to do (acs upgrade --version {v} installs that one)"
            ))
        }
        Decision::Install if opts.check => {
            return Ok(format!(
                "acs {v} is available (you have {current}) — run: acs upgrade{}",
                opts.version
                    .as_ref()
                    .map(|_| format!(" --version {v}"))
                    .unwrap_or_default()
            ))
        }
        Decision::Install => {}
    }

    let exe = crate::sys::self_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot find this acs binary: {e}"))?;
    let layout = Layout::detect(&exe, current, &link_candidates());
    for dir in layout.dirs(&v) {
        check_writable(&dir)?;
    }

    let new = download(&base, &asset, &target, &tmp.0)?;
    let dest = layout.destination(&v);
    let mode = std::fs::metadata(&exe)
        .map(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o7777
        })
        .unwrap_or(0o755);
    place(&new, &dest, mode)?;
    if let Layout::Versioned { links, .. } = &layout {
        for l in links {
            relink(&dest, l)?;
        }
    }
    let shown = match &layout {
        Layout::Versioned { links, .. } if !links.is_empty() => links
            .iter()
            .map(|l| l.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        _ => dest.display().to_string(),
    };
    Ok(format!("upgraded acs {current} → {v} ({shown})"))
}

/// Fail early, before downloading, if `dir` cannot take a new file.
fn check_writable(dir: &Path) -> Result<(), Error> {
    let probe = dir.join(format!(".acs-upgrade.{}", crate::sys::getpid()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(Error::Failed(format!(
            "cannot write to {} — re-run with sudo: sudo acs upgrade",
            dir.display()
        ))),
        Err(e) => Err(Error::Failed(format!(
            "cannot write to {}: {e}",
            dir.display()
        ))),
    }
}

/// Download, verify and unpack the release; returns the new binary's path.
fn download(base: &str, asset: &Asset, target: &str, tmp: &Path) -> Result<PathBuf, Error> {
    let archive = tmp.join(&asset.file);
    release::fetch(&release::asset_url(base, asset), &archive, 300)?;
    let data = std::fs::read(&archive).map_err(|e| e.to_string())?;
    let got = crate::sha256::hex(&crate::sha256::digest(&data));
    if got != asset.sha256 {
        return Err(Error::Failed(format!(
            "checksum mismatch for {} (expected {}, got {got}); nothing was changed",
            asset.file, asset.sha256
        )));
    }
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    let new = tmp
        .join(format!("acs-{}-{target}", asset.version))
        .join("acs");
    if !st.success() || !new.is_file() {
        return Err(Error::Failed(format!(
            "{} does not hold acs-{}-{target}/acs",
            asset.file, asset.version
        )));
    }
    // It must run here and be what it says.
    let out = Command::new(&new)
        .arg("--version")
        .output()
        .map_err(|e| format!("the downloaded acs does not run here: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.starts_with(&format!("acs {} ", asset.version)) {
        return Err(Error::Failed(format!(
            "the downloaded acs does not report version {} ({})",
            asset.version,
            text.lines().next().unwrap_or("no output")
        )));
    }
    Ok(new)
}

/// Copy `from` next to `to` under a temporary name, then rename it over
/// `to`: the path is never missing or half-written.
fn place(from: &Path, to: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = to.parent().ok_or("no directory to install into")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".acs.upgrade.{:08x}",
        crate::sys::random_u64() as u32
    ));
    let r = std::fs::copy(from, &tmp)
        .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)))
        .and_then(|_| std::fs::rename(&tmp, to));
    r.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot install {}: {e}", to.display())
    })
}

/// Point the symlink `link` at `target`, atomically.
fn relink(target: &Path, link: &Path) -> Result<(), String> {
    let dir = link.parent().ok_or("no directory for the link")?;
    let tmp = dir.join(format!(".acs.link.{:08x}", crate::sys::random_u64() as u32));
    std::os::unix::fs::symlink(target, &tmp)
        .and_then(|_| std::fs::rename(&tmp, link))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot repoint {}: {e}", link.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn args(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(parse(&args("")).unwrap(), Opts::default());
        let o = parse(&args("--check --version 1.2.3")).unwrap();
        assert!(o.check);
        assert_eq!(o.version.as_deref(), Some("1.2.3"));
        assert_eq!(
            parse(&args("--version=v0.4.0")).unwrap().version.as_deref(),
            Some("v0.4.0")
        );
        assert!(parse(&args("--version latest"))
            .unwrap_err()
            .contains("X.Y.Z"));
        assert!(parse(&args("--version")).is_err());
        assert!(parse(&args("now"))
            .unwrap_err()
            .contains("unexpected argument now"));
    }

    #[test]
    fn decisions() {
        assert_eq!(decide("0.2.0", "0.3.0", false), Decision::Install);
        assert_eq!(decide("0.2.0", "0.2.0", false), Decision::Current);
        assert_eq!(decide("0.2.0", "0.2.0", true), Decision::Current);
        // No downgrade unless asked for by version.
        assert_eq!(decide("0.3.0", "0.2.0", false), Decision::Newer);
        assert_eq!(decide("0.3.0", "0.2.0", true), Decision::Install);
        assert_eq!(decide("0.3.0-dev", "0.3.0", false), Decision::Install);
    }

    #[test]
    fn a_plain_binary_is_replaced_in_place() {
        let d = TempDir::new();
        let exe = d.path().join("acs");
        std::fs::write(&exe, "old").unwrap();
        let l = Layout::detect(&exe, "0.2.0", &[]);
        assert_eq!(l, Layout::Plain(exe.clone()));
        assert_eq!(l.destination("0.3.0"), exe);
    }

    #[test]
    fn a_versioned_install_gets_a_sibling_and_its_links_repointed() {
        let d = TempDir::new();
        let root = d.path().join("share/acs");
        let exe = root.join("0.2.0/acs");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "old").unwrap();
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let link = bin.join("acs");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        let other = bin.join("other");
        std::os::unix::fs::symlink(d.path(), &other).unwrap();
        let plain = bin.join("plain");
        std::fs::write(&plain, "x").unwrap();

        let exe = std::fs::canonicalize(&exe).unwrap();
        let l = Layout::detect(&exe, "0.2.0", &[link.clone(), other, plain, link.clone()]);
        let Layout::Versioned { root: r, links } = &l else {
            panic!("{l:?}")
        };
        assert_eq!(r, &std::fs::canonicalize(&root).unwrap());
        assert_eq!(links, std::slice::from_ref(&link));
        assert_eq!(l.destination("0.3.0"), r.join("0.3.0/acs"));

        // Installing and relinking.
        let new = d.path().join("new");
        std::fs::write(&new, "new").unwrap();
        place(&new, &l.destination("0.3.0"), 0o755).unwrap();
        relink(&l.destination("0.3.0"), &link).unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn another_versions_directory_is_not_versioned_for_us() {
        let d = TempDir::new();
        let exe = d.path().join("9.9.9/acs");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "x").unwrap();
        assert!(matches!(
            Layout::detect(&exe, "0.2.0", &[]),
            Layout::Plain(_)
        ));
    }
}
