//! Where this build publishes: the one place the release tasks ask, so a
//! fork sets `ACS_DEFAULT_RELEASES_URL` once and the Homebrew formula, the
//! release notes and the packaged installer all follow it (acs-x57,
//! docs/VERSIONING.md "Forking").
//!
//! xtask links the `acs` library, which `build.rs` builds with the same
//! variables as the binaries it is packaging, so
//! [`acs::release::DEFAULT_RELEASES_URL`] and
//! [`acs::signature::RELEASE_KEY`] here are the ones those binaries carry —
//! as long as `dist` and `package` are run in the same environment, which
//! `scripts/release-binaries.sh` does.

/// The tap `scripts/update-tap.sh` pushes to when `ACS_TAP_REPO` says
/// nothing. The script has the same default, and a test holds the two
/// together.
pub const UPSTREAM_TAP: &str = "https://github.com/michel-onstein/homebrew-acs.git";

/// The releases this build publishes to and updates from.
pub fn releases() -> &'static str {
    acs::release::DEFAULT_RELEASES_URL
}

/// The project page behind a releases URL: the URL without the `/releases`
/// that GitHub and GitLab both publish under. Anything else is its own
/// homepage.
pub fn repo(releases: &str) -> &str {
    let r = releases.trim_end_matches('/');
    r.strip_suffix("/releases").unwrap_or(r)
}

/// The `brew install` argument for the tap this release pipeline updates:
/// `<owner>/<tap>/acs`, where `<tap>` is the repository name without
/// Homebrew's `homebrew-` prefix.
///
/// `tap_repo` is `ACS_TAP_REPO` — the same variable `scripts/update-tap.sh`
/// takes, defaulting to the same tap — or `None` when the release skips the
/// tap (`ACS_NO_TAP`), and then the notes say nothing about Homebrew rather
/// than sending a fork's users to a formula nobody pushed.
pub fn brew_ref(tap_repo: Option<&str>) -> Option<String> {
    let repo = tap_repo?
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let mut parts = repo.rsplit('/');
    let name = parts.next().filter(|s| !s.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let tap = name.strip_prefix("homebrew-").unwrap_or(name);
    (!tap.is_empty()).then(|| format!("{owner}/{tap}/acs"))
}

/// What the environment says about the tap: `ACS_TAP_REPO`, upstream's tap
/// when it is unset, and nothing at all under `ACS_NO_TAP`.
pub fn tap_from_env() -> Option<String> {
    if std::env::var_os("ACS_NO_TAP").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    Some(
        std::env::var("ACS_TAP_REPO")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| UPSTREAM_TAP.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_homepage_is_the_releases_url_without_its_last_segment() {
        assert_eq!(
            repo("https://github.com/michel-onstein/acs/releases"),
            "https://github.com/michel-onstein/acs"
        );
        assert_eq!(
            repo("https://github.com/you/acs-fork/releases/"),
            "https://github.com/you/acs-fork"
        );
        // Not a `/releases` URL: it is its own homepage.
        assert_eq!(
            repo("https://dl.example.com/acs/"),
            "https://dl.example.com/acs"
        );
    }

    #[test]
    fn the_brew_argument_names_the_tap_that_is_pushed() {
        assert_eq!(
            brew_ref(Some(UPSTREAM_TAP)).unwrap(),
            "michel-onstein/acs/acs"
        );
        assert_eq!(
            brew_ref(Some("https://github.com/you/homebrew-acs.git")).unwrap(),
            "you/acs/acs"
        );
        // Without the conventional prefix, and without the .git.
        assert_eq!(
            brew_ref(Some("https://github.com/you/taps")).unwrap(),
            "you/taps/acs"
        );
        // A release that skips the tap says nothing about Homebrew, and
        // neither does a path that names no repository.
        assert_eq!(brew_ref(None), None);
        assert_eq!(brew_ref(Some("homebrew-acs")), None);
        assert_eq!(brew_ref(Some("")), None);
    }

    /// The default tap is written twice — here and in
    /// `scripts/update-tap.sh`, which is a shell script and cannot read
    /// this — so the notes cannot come to name a tap the release does not
    /// push to.
    #[test]
    fn the_default_tap_is_the_one_update_tap_sh_pushes_to() {
        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/update-tap.sh"),
        )
        .unwrap();
        assert!(
            script.contains(&format!("${{ACS_TAP_REPO:-{UPSTREAM_TAP}}}")),
            "update-tap.sh does not default to {UPSTREAM_TAP}"
        );
    }
}
