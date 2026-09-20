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

/// Where releases are published when nothing overrides it.
pub const DEFAULT_RELEASES_URL: &str = "https://github.com/michel-onstein/acs/releases";

/// Where releases are published, with `ACS_RELEASES_URL` honoured only
/// where it is safe to (acs-95w).
///
/// What is downloaded from here is checked against a `SHA256SUMS` fetched
/// from the same place and is then **run**, so whoever chooses this string
/// chooses what acs executes. Two limits:
///
/// - **Not across a privilege boundary.** Under `sudo -E`, a sudoers
///   `env_keep`, or anything else that leaves the real and effective uid
///   different, the variable is ignored outright. An attacker who can seed
///   the environment of a privileged run must not thereby choose the
///   binary that run installs.
/// - **https only**, unless the caller was told otherwise on the *command
///   line*. `http://` and `file://` bypass integrity checking entirely,
///   since the sums travel with the payload. The opt-out is deliberately
///   not an environment variable: whoever can set the URL could set that
///   too, and the check would be worth nothing.
pub fn releases_url(allow_insecure: bool) -> Result<String, String> {
    let set = std::env::var("ACS_RELEASES_URL")
        .ok()
        .filter(|v| !v.is_empty());
    let (url, warning) = choose_releases_url(
        set.as_deref(),
        crate::sys::privileges_dropped(),
        allow_insecure,
    )?;
    if let Some(w) = warning {
        eprintln!("acs: {w}");
    }
    Ok(url)
}

/// The decision [`releases_url`] makes, without reading the environment or
/// the process's uids, so both branches can be tested.
///
/// Returns the base to use and, where the override was dropped, what to say
/// about it.
pub fn choose_releases_url(
    set: Option<&str>,
    privileges_dropped: bool,
    allow_insecure: bool,
) -> Result<(String, Option<String>), String> {
    let Some(url) = set.filter(|v| !v.is_empty()) else {
        return Ok((DEFAULT_RELEASES_URL.to_string(), None));
    };
    if privileges_dropped {
        return Ok((
            DEFAULT_RELEASES_URL.to_string(),
            Some(format!(
                "ignoring ACS_RELEASES_URL: the real and effective user differ, \
                 so releases come from {DEFAULT_RELEASES_URL}"
            )),
        ));
    }
    if !url.starts_with("https://") && !allow_insecure {
        return Err(format!(
            "ACS_RELEASES_URL is {}, which cannot be authenticated: what is downloaded is checked \
             against a SHA256SUMS from the same place, and then run. Use https://, or pass \
             --allow-insecure-url to accept it.",
            scheme_of(url)
        ));
    }
    Ok((url.trim_end_matches('/').to_string(), None))
}

/// The scheme of `url` for an error message, or the whole string when it
/// has none.
fn scheme_of(url: &str) -> &str {
    match url.split_once("://") {
        Some((s, _)) => s,
        None => url,
    }
}

/// The release archive target for this build: Linux builds are published
/// as static musl binaries, whatever libc this one was built against.
pub fn archive_target(own: &str) -> String {
    match own.split_once("-unknown-linux-") {
        // Keep the ABI suffix: armv7's musl triple is musleabihf, and
        // armv7-unknown-linux-musl is not a target at all (acs-ad5).
        Some((arch, libc)) => {
            let abi = libc
                .strip_prefix("gnu")
                .or_else(|| libc.strip_prefix("musl"))
                .unwrap_or("");
            format!("{arch}-unknown-linux-musl{abi}")
        }
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
        (Some(x), Some(y)) => compare_pre(x, y),
    }))
}

/// Pre-release order: identifiers (split on `.`) field by field, a numeric
/// one below an alphanumeric one and a longer pre-release above a shorter
/// one that it starts with, as semver says. Within a field, runs of digits
/// compare as numbers, so `rc2` is below `rc10` as a person reads it —
/// strict semver would compare that pair as text (acs-ad5).
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut x = a.split('.');
    let mut y = b.split('.');
    loop {
        let ord = match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => match (numeric(p), numeric(q)) {
                (Some(m), Some(n)) => m.cmp(&n),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => natural(p, q),
            },
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

/// The identifier as a number, if it is all digits.
fn numeric(s: &str) -> Option<u64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// Text order with runs of digits compared as numbers: `rc2` < `rc10`.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        let ord = match (x.first(), y.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let (m, rx) = take_digits(x);
                let (n, ry) = take_digits(y);
                x = rx;
                y = ry;
                m.cmp(&n)
            }
            (Some(p), Some(q)) => {
                x = &x[1..];
                y = &y[1..];
                p.cmp(q)
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

/// The leading run of digits as a number (saturating), and the rest.
fn take_digits(s: &[u8]) -> (u64, &[u8]) {
    let end = s
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    let n = std::str::from_utf8(&s[..end])
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(u64::MAX);
    (n, &s[end..])
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

    /// acs-95w: what `ACS_RELEASES_URL` names is downloaded, checked only
    /// against sums fetched from the same place, and then run. A scheme
    /// that cannot be authenticated therefore needs an explicit say-so.
    #[test]
    fn a_releases_url_that_cannot_be_authenticated_needs_the_flag() {
        // Unset, or empty: the published releases, over https.
        assert_eq!(
            choose_releases_url(None, false, false).unwrap(),
            (DEFAULT_RELEASES_URL.to_string(), None)
        );
        assert_eq!(
            choose_releases_url(Some(""), false, false).unwrap(),
            (DEFAULT_RELEASES_URL.to_string(), None)
        );

        for bad in [
            "http://mirror.example/releases",
            "file:///tmp/mirror",
            "ftp://mirror.example",
            "/tmp/mirror",
        ] {
            let e = choose_releases_url(Some(bad), false, false).unwrap_err();
            assert!(e.contains("cannot be authenticated"), "{bad}: {e}");
            assert!(e.contains("--allow-insecure-url"), "{bad}: {e}");
            // With the flag it is taken, trailing slash trimmed.
            let (url, warn) = choose_releases_url(Some(bad), false, true).unwrap();
            assert_eq!(url, bad.trim_end_matches('/'));
            assert_eq!(warn, None);
        }

        // https needs no flag.
        let (url, warn) =
            choose_releases_url(Some("https://mirror.example/r/"), false, false).unwrap();
        assert_eq!(url, "https://mirror.example/r");
        assert_eq!(warn, None);
    }

    /// acs-95w: across a privilege boundary the variable is ignored
    /// outright — with the flag, with https, with anything. Whoever seeds
    /// the environment of a sudo run must not choose what it installs.
    #[test]
    fn a_releases_url_is_ignored_when_the_real_and_effective_user_differ() {
        for (set, allow) in [
            ("http://mirror.example", false),
            ("http://mirror.example", true),
            ("file:///tmp/mirror", true),
            ("https://mirror.example", false),
        ] {
            let (url, warn) = choose_releases_url(Some(set), true, allow).unwrap();
            assert_eq!(url, DEFAULT_RELEASES_URL, "{set} was honoured under sudo");
            let warn = warn.expect("the user is told the setting was dropped");
            assert!(warn.contains("ignoring ACS_RELEASES_URL"), "{warn}");
        }
    }

    /// acs-95w: the error names the scheme, so the reader can see which
    /// part of their setting is the problem.
    #[test]
    fn the_insecure_url_error_names_the_scheme() {
        let e = choose_releases_url(Some("http://mirror.example"), false, false).unwrap_err();
        assert!(e.contains("is http,"), "{e}");
        let e = choose_releases_url(Some("file:///tmp/x"), false, false).unwrap_err();
        assert!(e.contains("is file,"), "{e}");
    }

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
        // The ABI suffix stays: there is no armv7-unknown-linux-musl.
        assert_eq!(
            archive_target("armv7-unknown-linux-musleabihf"),
            "armv7-unknown-linux-musleabihf"
        );
        assert_eq!(
            archive_target("armv7-unknown-linux-gnueabihf"),
            "armv7-unknown-linux-musleabihf"
        );
    }

    /// Regression (acs-ad5): pre-releases were compared as text, so
    /// 0.3.0-rc10 sorted below 0.3.0-rc2.
    #[test]
    fn pre_releases_compare_by_number_not_by_text() {
        use Ordering::*;
        assert_eq!(compare("0.3.0-rc2", "0.3.0-rc10"), Some(Less));
        assert_eq!(compare("0.3.0-rc.2", "0.3.0-rc.10"), Some(Less));
        assert_eq!(compare("0.3.0-rc10", "0.3.0-rc10"), Some(Equal));
        // Semver's own rules still hold.
        assert_eq!(compare("0.3.0-alpha", "0.3.0-alpha.1"), Some(Less));
        assert_eq!(compare("0.3.0-1", "0.3.0-alpha"), Some(Less));
        assert_eq!(compare("0.3.0-alpha.2", "0.3.0-beta.1"), Some(Less));
        assert_eq!(compare("0.3.0-dev", "0.3.0-rc1"), Some(Less));
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
