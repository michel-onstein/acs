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
//! A Homebrew install (the binary is in a `Cellar`) is brew's to replace:
//! `acs upgrade` says to run `brew upgrade acs` instead.
//!
//! Remote hosts need nothing: the next connection finds no binary of the new
//! version there and installs it (DESIGN §8).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::release::{self, Asset};

pub const USAGE: &str = "\
usage: acs upgrade [--version X.Y.Z] [--check] [--allow-insecure-url]

Replace this acs with the latest release from GitHub (checked against the
release's SHA256SUMS). Remote hosts get the new version on the next connect.

  --version X.Y.Z       install that release, even an older one
  --check               only say whether a newer release exists
  --allow-insecure-url  accept an ACS_RELEASES_URL that is not https, which
                        cannot be authenticated (the sums travel with the
                        payload). ACS_RELEASES_URL is ignored altogether
                        when the real and effective user differ.";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Opts {
    pub version: Option<String>,
    pub check: bool,
    pub help: bool,
    /// Accept an `ACS_RELEASES_URL` that is not https (acs-95w). A command
    /// line flag on purpose: an environment variable would be set by
    /// whoever set the URL.
    pub allow_insecure_url: bool,
}

pub fn parse(args: &[OsString]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--allow-insecure-url" => o.allow_insecure_url = true,
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

/// Whether `exe`, a real path, is in a Homebrew keg:
/// `<prefix>/Cellar/acs/<version>/bin/acs` for any prefix (`/opt/homebrew`,
/// `/usr/local`, `/home/linuxbrew/.linuxbrew`).
pub fn brewed(exe: &Path) -> bool {
    let parts: Vec<&std::ffi::OsStr> = exe.components().map(|c| c.as_os_str()).collect();
    parts.windows(2).any(|w| w[0] == "Cellar" && w[1] == "acs")
}

/// The command that upgrades the running acs.
pub fn command() -> &'static str {
    let exe = crate::sys::self_exe().and_then(std::fs::canonicalize);
    if exe.is_ok_and(|e| brewed(&e)) {
        "brew upgrade acs"
    } else {
        "acs upgrade"
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
    if let Some(home) = crate::sys::env_nonempty("HOME") {
        v.push(crate::layout::user_bin(Path::new(&home)).join("acs"));
    }
    v.push(crate::layout::system_link());
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

/// Create a fresh `<prefix>.<token>` directory under the temp directory,
/// 0700 whatever the umask (acs-3sae). What is downloaded into it is
/// verified, re-read, unpacked and run by path, so a directory someone else
/// could write — 0775 under umask 002 with a shared group — would let them
/// swap a file between its check and its use. The mode is still masked by
/// the umask, but no umask in use takes the owner's own bits.
pub(crate) fn scratch_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let p = std::env::temp_dir().join(format!("{prefix}.{}", crate::sys::random_token()));
    std::fs::DirBuilder::new().mode(0o700).create(&p)?;
    Ok(p)
}

/// A temporary directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        scratch_dir("acs-upgrade")
            .map(Scratch)
            .map_err(|e| format!("cannot create a scratch directory: {e}"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(opts: &Opts) -> Result<String, Error> {
    let current = crate::VERSION;
    let exe = crate::sys::self_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("cannot find this acs binary: {e}"));
    let brew = exe.as_deref().is_ok_and(brewed);
    if let (true, false, Ok(e)) = (brew, opts.check, &exe) {
        return Err(Error::Failed(format!(
            "this acs was installed with Homebrew ({}) — upgrade it with: brew upgrade acs",
            e.display()
        )));
    }
    let target = release::own_archive_target();
    let base = release::releases_url(opts.allow_insecure_url).map_err(Error::Failed)?;
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
            let how = match opts.version {
                _ if brew => "brew upgrade acs".to_string(),
                Some(_) => format!("acs upgrade --version {v}"),
                None => "acs upgrade".to_string(),
            };
            return Ok(format!("acs {v} is available (you have {current}) — run: {how}"));
        }
        Decision::Install => {}
    }

    let exe = exe?;
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
    let staged = Staged::new(&new, &dest, mode)?;
    check_runs(staged.path(), &v)?;
    staged.commit(&dest)?;
    if let Layout::Versioned { links, .. } = &layout {
        for l in links {
            relink(&dest, l)?;
            link_copy_beside(l);
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
    let probe = dir.join(format!(".acs-upgrade.{}", crate::sys::random_token()));
    match crate::sys::create_new(&probe) {
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
    let got = crate::sha256::file(&archive).map_err(|e| e.to_string())?;
    let got = crate::sha256::hex(&got);
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
    Ok(new)
}

/// Run the new binary where it will live and check what it says: proof
/// that it works on this machine, from the directory it will work from.
/// Running it from the scratch directory instead would fail wherever /tmp
/// is mounted noexec, common hardening the upgrade must not trip on
/// (acs-x1k).
fn check_runs(new: &Path, version: &str) -> Result<(), Error> {
    let out = Command::new(new).arg("--version").output().map_err(|e| {
        let dir = new.parent().unwrap_or(Path::new(".")).display();
        format!("the downloaded acs does not run from {dir}: {e} (is it mounted noexec?)")
    })?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.starts_with(&format!("acs {version} ")) {
        return Err(Error::Failed(format!(
            "the downloaded acs does not report version {version} ({})",
            text.lines().next().unwrap_or("no output")
        )));
    }
    Ok(())
}

/// The new binary beside its destination under a temporary name, removed
/// again unless it is committed. Kept apart so it can be run from there
/// before the rename.
struct Staged {
    tmp: PathBuf,
    committed: bool,
}

impl Staged {
    /// Copy `from` next to `to` under a temporary name, with `mode`.
    fn new(from: &Path, to: &Path, mode: u32) -> Result<Staged, String> {
        let dir = to.parent().ok_or("no directory to install into")?;
        make_dirs(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // Created by us, or not at all: `fs::copy` opens the destination
        // with create-and-truncate, which follows a symlink planted at
        // that name and writes through to whatever it points at — as root,
        // when the user was told to re-run under sudo (acs-721).
        let tmp = dir.join(format!(".acs.upgrade.{}", crate::sys::random_token()));
        (|| -> std::io::Result<()> {
            let mut out = crate::sys::create_new_mode(&tmp, mode)?;
            let mut src = std::fs::File::open(from)?;
            std::io::copy(&mut src, &mut out).map(drop)
        })()
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot install {}: {e}", to.display())
        })?;
        Ok(Staged {
            tmp,
            committed: false,
        })
    }

    fn path(&self) -> &Path {
        &self.tmp
    }

    /// Rename it over `to`: the path is never missing or half-written.
    fn commit(mut self, to: &Path) -> Result<(), String> {
        std::fs::rename(&self.tmp, to)
            .map_err(|e| format!("cannot install {}: {e}", to.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Create `dir` and any missing parents as 0755 whatever the umask, as the
/// remote install's `make_dir` does (acs-91q2). The remote prelude refuses a
/// binary whose directory others can write, so `root/<version>/` made 0775
/// under umask 002 (Ubuntu's default with user-private groups) was refused
/// once and re-uploaded on the first connection to an upgraded host.
/// Directories that already exist are left alone: in the plain layout that
/// is wherever the binary lives, which is not ours to change.
fn make_dirs(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)?;
    // The mode above is still masked by the umask; set it outright, so a
    // stricter one than 022 cannot leave a system install that only root
    // can read.
    for d in missing.iter().rev() {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Point the symlink `link` at `target`, atomically.
fn relink(target: &Path, link: &Path) -> Result<(), String> {
    crate::sys::symlink_atomic(target, link)
        .map_err(|e| format!("cannot repoint {}: {e}", link.display()))
}

/// `acscp -> acs` beside a link `link` this upgrade repointed: the
/// relative link already follows the new `acs`, and one missing (an
/// install from before acscp) is made (ACSCP_DESIGN §9). A failure is a
/// warning — `acs cp` needs no link — not a failed upgrade. A link by
/// another name (acs run as `~/bin/a`) gets none: `acscp -> acs` beside it
/// would point at nothing.
fn link_copy_beside(link: &Path) {
    if link.file_name() != Some(std::ffi::OsStr::new("acs")) {
        return;
    }
    let Some(dir) = link.parent() else { return };
    if let Err(e) = crate::install::link_copy_name(dir) {
        eprintln!("acs: warning: cannot link {e} (`acs cp` works without it)");
    }
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
        // Either spelling: release::release_tag maps both to the tag.
        assert_eq!(
            parse(&args("--version=v0.4.0")).unwrap().version.as_deref(),
            Some("v0.4.0")
        );
        assert_eq!(
            parse(&args("--version=0.19.0")).unwrap().version.as_deref(),
            Some("0.19.0")
        );
        assert!(parse(&args("--version latest"))
            .unwrap_err()
            .contains("X.Y.Z"));
        assert!(parse(&args("--version")).is_err());
        assert!(parse(&args("now"))
            .unwrap_err()
            .contains("unexpected argument now"));
    }

    /// acs-3sae: the scratch directory was `create_dir`'d at 0777 & ~umask —
    /// world-readable under 022, group-writable under 002. The umask is not
    /// changed here, as it belongs to every test in the process;
    /// `tests/upgrade.rs` runs `acs upgrade` under 002.
    #[test]
    fn the_scratch_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let p = scratch_dir("acs-test-scratch").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir(&p).unwrap();
        assert_eq!(mode, 0o700, "{mode:o}");
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
        let staged = Staged::new(&new, &l.destination("0.3.0"), 0o755).unwrap();
        assert!(staged.path().exists());
        staged.commit(&l.destination("0.3.0")).unwrap();
        relink(&l.destination("0.3.0"), &link).unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    /// acs-hofu.5: an upgrade of an install from before acscp makes the
    /// `acscp -> acs` link beside the `acs` it repointed; a link by another
    /// name gets none, and one already there keeps following `acs`.
    #[test]
    fn an_upgrade_links_acscp_beside_the_acs_it_repointed() {
        let d = TempDir::new();
        let bin = d.path();
        std::fs::write(bin.join("acs"), "new").unwrap();
        let copy = bin.join(crate::layout::COPY_NAME);

        link_copy_beside(&bin.join("myacs"));
        assert!(std::fs::symlink_metadata(&copy).is_err(), "not beside acs");

        link_copy_beside(&bin.join("acs"));
        assert_eq!(std::fs::read_link(&copy).unwrap(), Path::new("acs"));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "new");
        link_copy_beside(&bin.join("acs"));
        assert_eq!(std::fs::read_link(&copy).unwrap(), Path::new("acs"));
    }

    /// A staged copy that is not committed leaves nothing behind (acs-x1k).
    #[test]
    fn a_staged_copy_that_is_not_committed_is_removed() {
        let d = TempDir::new();
        let new = d.path().join("new");
        std::fs::write(&new, "new").unwrap();
        let dest = d.path().join("dir/acs");
        let tmp = {
            let staged = Staged::new(&new, &dest, 0o755).unwrap();
            staged.path().to_path_buf()
        };
        assert!(!tmp.exists(), "{} was left behind", tmp.display());
        assert!(!dest.exists());
    }

    #[test]
    fn a_homebrew_keg_is_brewed_under_any_prefix() {
        for p in [
            "/opt/homebrew/Cellar/acs/0.3.0/bin/acs",
            "/usr/local/Cellar/acs/0.3.0/bin/acs",
            "/home/linuxbrew/.linuxbrew/Cellar/acs/0.3.0/bin/acs",
            "/Users/me/homebrew/Cellar/acs/0.3.0_1/bin/acs",
        ] {
            assert!(brewed(Path::new(p)), "{p}");
        }
        for p in [
            "/Users/me/.local/share/acs/0.3.0/acs",
            "/usr/local/lib/acs/0.3.0/acs",
            "/usr/local/bin/acs",
            "/opt/homebrew/bin/acs",
            // Another formula's keg, or a directory merely named like one.
            "/opt/homebrew/Cellar/other/1.0/bin/acs",
            "/home/me/Cellars/acs/0.3.0/bin/acs",
            "/home/me/my-Cellar/acs/acs",
        ] {
            assert!(!brewed(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn a_brewed_acs_is_found_through_the_brew_link() {
        let d = TempDir::new();
        let exe = d.path().join("homebrew/Cellar/acs/0.3.0/bin/acs");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "acs").unwrap();
        let bin = d.path().join("homebrew/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("../Cellar/acs/0.3.0/bin/acs", bin.join("acs")).unwrap();
        // `bin/acs` is a link; its real path is the keg's.
        assert!(!brewed(&bin.join("acs")));
        assert!(brewed(&std::fs::canonicalize(bin.join("acs")).unwrap()));
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
