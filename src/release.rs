//! Published releases (docs/VERSIONING.md, "Binaries"): which archive is
//! ours, what the newest version is, and downloading with `curl` (DESIGN
//! §7.5). acs has no HTTP or TLS client of its own — one would cost more
//! than the rest of the binary (DESIGN §9).
//!
//! Like the one-line installer (`scripts/install.sh`), everything is read
//! from a release's `SHA256SUMS`: its archive names carry the version, so the
//! same small file answers "what is the newest release" and "what must the
//! download hash to", without the GitHub API or its rate limit.

use std::cmp::Ordering;
use std::path::Path;
use std::process::{Command, Stdio};

/// Where releases are published (`ACS_RELEASES_URL` overrides it).
pub fn releases_url() -> String {
    std::env::var("ACS_RELEASES_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://github.com/michel-onstein/acs/releases".into())
        .trim_end_matches('/')
        .to_string()
}

/// The release archive target for this build: Linux builds are published
/// as static musl binaries, whatever libc this one was built against.
pub fn archive_target(own: &str) -> String {
    match own.split_once("-unknown-linux-") {
        Some((arch, _)) => format!("{arch}-unknown-linux-musl"),
        None => own.to_string(),
    }
}

/// Our archive in a release, from its `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub version: String,
    pub file: String,
    pub sha256: String,
}

/// Find the archive for `target` in a `SHA256SUMS` text.
pub fn find_asset(sums: &str, target: &str) -> Option<Asset> {
    let suffix = format!("-{target}.tar.gz");
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(' ')?;
        let file = file.trim_start_matches([' ', '*']);
        let version = file.strip_prefix("acs-")?.strip_suffix(&suffix)?;
        let hex = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        // Releases are X.Y.Z; a `-` in the middle would be part of another
        // target's name, not a pre-release.
        let plain = parse_version(version).is_some_and(|(_, pre)| pre.is_none());
        (hex && plain).then(|| Asset {
            version: version.to_string(),
            file: file.to_string(),
            sha256: hash.to_ascii_lowercase(),
        })
    })
}

/// `X.Y.Z` or `X.Y.Z-pre` (a leading `v` is allowed): the numbers and the
/// pre-release part.
pub fn parse_version(v: &str) -> Option<([u64; 3], Option<&str>)> {
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (v, None),
    };
    let mut n = [0u64; 3];
    let mut parts = core.split('.');
    for slot in &mut n {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some((n, pre))
}

/// Semantic-version order; a pre-release sorts before its release. `None`
/// if either is not a version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    let (na, pa) = parse_version(a)?;
    let (nb, pb) = parse_version(b)?;
    Some(na.cmp(&nb).then(match (pa, pb) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    }))
}

/// `version` without a leading `v`.
pub fn bare(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// The `SHA256SUMS` URL of the latest release, or of `version`.
pub fn sums_url(base: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{base}/download/v{}/SHA256SUMS", bare(v)),
        None => format!("{base}/latest/download/SHA256SUMS"),
    }
}

pub fn asset_url(base: &str, a: &Asset) -> String {
    format!("{base}/download/v{}/{}", a.version, a.file)
}

/// Download `url` into `dest` with curl (or wget), giving up after
/// `timeout_secs`. The error says what failed, not how.
pub fn fetch(url: &str, dest: &Path, timeout_secs: u32) -> Result<(), String> {
    let t = timeout_secs.to_string();
    let curl = || {
        Command::new("curl")
            .args(["-fsSL", "--max-time", &t, "-o"])
            .arg(dest)
            .arg(url)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    let wget = || {
        Command::new("wget")
            .args(["-q", "-T", &t, "-O"])
            .arg(dest)
            .arg(url)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    let status = match curl() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => wget(),
        other => other,
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(format!("cannot download {url}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("acs needs curl (or wget) to reach GitHub".into())
        }
        Err(e) => Err(format!("cannot run curl: {e}")),
    }
}

/// Download and read a release's `SHA256SUMS`; find our archive in it.
pub fn lookup(
    base: &str,
    version: Option<&str>,
    target: &str,
    tmp: &Path,
    timeout_secs: u32,
) -> Result<Asset, String> {
    let url = sums_url(base, version);
    let dest = tmp.join("SHA256SUMS");
    fetch(&url, &dest, timeout_secs).map_err(|e| match version {
        Some(v) if e.starts_with("cannot download") => {
            format!("{e} (is {} a release?)", bare(v))
        }
        _ => e,
    })?;
    let sums = std::fs::read_to_string(&dest).map_err(|e| format!("{url}: {e}"))?;
    let asset = find_asset(&sums, target)
        .ok_or_else(|| format!("the release has no build for {target}"))?;
    if let Some(v) = version {
        if asset.version != bare(v) {
            return Err(format!(
                "asked for {}, but the release holds {}",
                bare(v),
                asset.version
            ));
        }
    }
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &str = "\
1561565a626eb353fb2f0035ffddad3b508450ca49d049bd3af92d03fc0d40c9  acs-0.2.0-aarch64-apple-darwin.tar.gz
016af4f23a5db7b94b08bf20ec466fbc4931eb12ce9db76678a27570ea3ff0e1  acs-0.2.0-aarch64-unknown-linux-musl.tar.gz
01a4fa0bcc616dd4ba527b652b5f39d9bcbd91558d7c39a5f8b153d2b28787f7 *acs-0.2.0-x86_64-apple-darwin.tar.gz
nothexnothexnothexnothexnothexnothexnothexnothexnothexnothexnoth  acs-0.2.0-x86_64-unknown-linux-musl.tar.gz
";

    #[test]
    fn finds_our_archive_in_sha256sums() {
        let a = find_asset(SUMS, "aarch64-apple-darwin").unwrap();
        assert_eq!(a.version, "0.2.0");
        assert_eq!(a.file, "acs-0.2.0-aarch64-apple-darwin.tar.gz");
        assert!(a.sha256.starts_with("1561565a"));
        // Binary-mode marker.
        let a = find_asset(SUMS, "x86_64-apple-darwin").unwrap();
        assert_eq!(a.file, "acs-0.2.0-x86_64-apple-darwin.tar.gz");
        // A line with a bad hash does not count, nor does a missing target.
        assert_eq!(find_asset(SUMS, "x86_64-unknown-linux-musl"), None);
        assert_eq!(find_asset(SUMS, "riscv64gc-unknown-linux-musl"), None);
        // Not confused by a target that ends like another.
        assert_eq!(find_asset(SUMS, "apple-darwin"), None);
    }

    #[test]
    fn linux_builds_map_to_the_musl_archive() {
        assert_eq!(
            archive_target("x86_64-unknown-linux-gnu"),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            archive_target("aarch64-unknown-linux-musl"),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(
            archive_target("aarch64-apple-darwin"),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn versions_compare_semantically() {
        use Ordering::*;
        assert_eq!(compare("0.2.0", "0.10.0"), Some(Less));
        assert_eq!(compare("1.0.0", "0.99.99"), Some(Greater));
        assert_eq!(compare("v0.3.0", "0.3.0"), Some(Equal));
        assert_eq!(compare("0.3.0-rc1", "0.3.0"), Some(Less));
        assert_eq!(compare("0.3.0-rc2", "0.3.0-rc1"), Some(Greater));
        assert_eq!(compare("0.3", "0.3.0"), None);
        assert_eq!(compare("0.3.0.1", "0.3.0"), None);
        assert_eq!(compare("x.y.z", "0.3.0"), None);
        assert_eq!(compare("0.3.0-", "0.3.0"), None);
    }

    #[test]
    fn urls() {
        let b = "https://example.com/r";
        assert_eq!(
            sums_url(b, None),
            "https://example.com/r/latest/download/SHA256SUMS"
        );
        assert_eq!(
            sums_url(b, Some("v1.2.3")),
            "https://example.com/r/download/v1.2.3/SHA256SUMS"
        );
        let a = find_asset(SUMS, "aarch64-apple-darwin").unwrap();
        assert_eq!(
            asset_url(b, &a),
            "https://example.com/r/download/v0.2.0/acs-0.2.0-aarch64-apple-darwin.tar.gz"
        );
    }
}
