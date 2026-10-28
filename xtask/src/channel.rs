//! The release channel a `dist/` was built for — its releases URL and
//! release key — recorded by `cargo xtask dist` as `channel.stamp` and
//! checked by `cargo xtask package` before it packages anything (acs-u3qn).
//!
//! xtask reads both values at **its** compile time
//! ([`crate::upstream::releases`], [`acs::signature::RELEASE_KEY`]), so
//! `dist` run with a fork's `ACS_DEFAULT_RELEASES_URL` and `package` run
//! without it would put the fork's binaries beside acs's installer, notes
//! and formula. The stamp makes that a refusal instead of a release.
//!
//! It is a file of its own rather than a second line in `source.stamp`:
//! `scripts/e2e_ssh.sh --no-build` compares that one whole against
//! `scripts/source_stamp.sh`, and which channel a build is for says nothing
//! about whether it is this tree's.

use std::path::Path;

/// The file in a `dist/` that names its channel.
pub const FILE: &str = "channel.stamp";

/// Where a build publishes and the key its releases are signed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub releases: String,
    pub key: String,
}

impl Channel {
    /// The channel this xtask was compiled for: the one `package` writes
    /// into the installer, the notes and the formula.
    pub fn built() -> Channel {
        Channel {
            releases: crate::upstream::releases().to_string(),
            key: acs::signature::RELEASE_KEY.to_string(),
        }
    }

    /// The channel `build.rs` gives the binaries `dist` is about to build,
    /// from the values of `ACS_DEFAULT_RELEASES_URL` and
    /// `ACS_DEFAULT_RELEASE_KEY`: read by the same code `build.rs` reads them
    /// with (`src/build_channel.rs`), so trimmed, blank counting as unset,
    /// unset meaning acs's own — and `Err` with the build's own message for
    /// a value the build would refuse (acs-d7x).
    pub fn from_env(releases: Option<&str>, key: Option<&str>) -> Result<Channel, String> {
        use crate::build_channel::{check_release_key, check_releases_url};
        Ok(Channel {
            releases: check_releases_url(releases)?
                .unwrap_or(acs::release::UPSTREAM_RELEASES_URL)
                .to_string(),
            key: check_release_key(key)?
                .unwrap_or(acs::signature::UPSTREAM_RELEASE_KEY)
                .to_string(),
        })
    }

    /// The stamp's text: one `releases` line and one `key` line.
    pub fn stamp(&self) -> String {
        format!("releases {}\nkey {}\n", self.releases, self.key)
    }

    /// A stamp read back, or `None` when it is not one.
    pub fn parse(text: &str) -> Option<Channel> {
        let mut releases = None;
        let mut key = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("releases ") {
                releases.get_or_insert(v.to_string());
            } else if let Some(v) = line.strip_prefix("key ") {
                key.get_or_insert(v.to_string());
            }
        }
        Some(Channel {
            releases: releases?,
            key: key?,
        })
    }
}

/// `Ok` when the `dist/` at `dist` was built for `want`; otherwise why it
/// must not be packaged.
pub fn check(dist: &Path, want: &Channel) -> Result<(), String> {
    let path = dist.join(FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e} — not `cargo xtask dist` output, or from before it recorded its \
             channel; rebuild it (acs-u3qn)",
            path.display()
        )
    })?;
    let have = Channel::parse(&text)
        .ok_or_else(|| format!("{}: not a channel stamp: {text:?}", path.display()))?;
    if have == *want {
        return Ok(());
    }
    let mut why = format!(
        "{} was built for another release channel than this package would publish:\n",
        dist.display()
    );
    if have.releases != want.releases {
        why.push_str(&format!(
            "  releases: built for {}, packaging for {}\n",
            have.releases, want.releases
        ));
    }
    if have.key != want.key {
        why.push_str(&format!(
            "  release key: built for {}, packaging for {}\n",
            have.key, want.key
        ));
    }
    why.push_str(
        "run `cargo xtask dist` and `cargo xtask package` with the same \
         ACS_DEFAULT_RELEASES_URL and ACS_DEFAULT_RELEASE_KEY \
         (scripts/release-binaries.sh does; docs/VERSIONING.md, \"Forking\")",
    );
    Err(why)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORK_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork";

    fn fork() -> Channel {
        Channel {
            releases: "https://dl.example.com/acs".into(),
            key: FORK_KEY.into(),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("acs-channel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_stamp_reads_back_as_what_was_written() {
        for c in [Channel::built(), fork()] {
            assert_eq!(Channel::parse(&c.stamp()), Some(c));
        }
        assert_eq!(Channel::parse("releases https://x\n"), None);
        assert_eq!(Channel::parse(""), None);
    }

    /// What `build.rs` makes of the two variables, read the same way.
    #[test]
    fn the_environment_names_the_channel_build_rs_bakes_in() {
        let upstream = Channel::from_env(None, None).unwrap();
        assert_eq!(upstream.releases, acs::release::UPSTREAM_RELEASES_URL);
        assert_eq!(upstream.key, acs::signature::UPSTREAM_RELEASE_KEY);
        assert_eq!(Channel::from_env(Some("  "), Some("")), Ok(upstream));
        let f = Channel::from_env(
            Some(" https://dl.example.com/acs/ "),
            Some(&format!(" {FORK_KEY}\n")),
        );
        assert_eq!(f, Ok(fork()));
    }

    /// acs-d7x: a value the build would refuse is refused here too, with
    /// the build's own message, rather than stamped on a `dist/` that the
    /// build itself is about to fail.
    #[test]
    fn a_channel_the_build_would_refuse_is_refused() {
        let why = Channel::from_env(Some("http://dl.example.com/acs"), Some(FORK_KEY)).unwrap_err();
        assert!(
            why.contains("ACS_DEFAULT_RELEASES_URL must be one https URL"),
            "{why}"
        );
        let why = Channel::from_env(Some("https://dl.example.com/acs"), Some("ssh-ed25519 AAAA"))
            .unwrap_err();
        assert!(
            why.contains("ACS_DEFAULT_RELEASE_KEY must be one ssh public key line"),
            "{why}"
        );
    }

    #[test]
    fn a_dist_for_the_same_channel_is_packaged() {
        let d = scratch("same");
        std::fs::write(d.join(FILE), fork().stamp()).unwrap();
        check(&d, &fork()).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// acs-u3qn: a fork's binaries beside acs's installer — or acs's beside
    /// a fork's — is refused, naming what differs.
    #[test]
    fn a_dist_for_another_channel_is_refused() {
        let d = scratch("other");
        let upstream = Channel::from_env(None, None).unwrap();
        std::fs::write(d.join(FILE), fork().stamp()).unwrap();
        let why = check(&d, &upstream).unwrap_err();
        assert!(
            why.contains("built for https://dl.example.com/acs"),
            "{why}"
        );
        assert!(
            why.contains(&format!("packaging for {}", upstream.releases)),
            "{why}"
        );
        assert!(why.contains("release key: built for ssh-ed25519"), "{why}");

        // Only the key differing is a mismatch too, and says only that.
        let same_url = Channel {
            releases: fork().releases,
            key: upstream.key.clone(),
        };
        let why = check(&d, &same_url).unwrap_err();
        assert!(!why.contains("  releases:"), "{why}");
        assert!(why.contains("release key:"), "{why}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_dist_without_a_stamp_is_refused() {
        let d = scratch("none");
        let why = check(&d, &fork()).unwrap_err();
        assert!(why.contains("channel.stamp"), "{why}");
        std::fs::write(d.join(FILE), "garbage\n").unwrap();
        let why = check(&d, &fork()).unwrap_err();
        assert!(why.contains("not a channel stamp"), "{why}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
