// What `ACS_DEFAULT_RELEASE_KEY` means for a build, shared by `build.rs` —
// which `include!`s this file, a build script being no part of the crate —
// and by the tests below. Build scripts are not a test target, so a check
// that lives only in `build.rs` is exercised only by whoever runs the build
// by hand; put here, it runs on every `cargo test` (acs-okz).
//
// Nothing in here may name the crate or pull anything in: it is compiled
// twice, once inside `acs` and once inside a build script that shares none
// of its dependencies.

/// What `ACS_DEFAULT_RELEASE_KEY` tells this build to bake in: `None` when
/// it is unset or blank — the usual case, which leaves
/// `signature::UPSTREAM_RELEASE_KEY` in place, so neither can produce a
/// build with nothing to verify against — `Some` with the key otherwise,
/// and `Err` with the message the build dies with when the value is not one
/// ssh public key line, as `ssh-keygen` writes into a `.pub` file.
///
/// The key *is* the check (`src/signature.rs`): a mistyped one does not
/// weaken verification — every release then fails to verify, which is the
/// safe direction — but it is a whole release nobody can install, found by
/// a user rather than by the person who built it. A second line would be a
/// second allowed signer, which nobody means to write, and would corrupt
/// the `cargo:rustc-env=` directive besides. Cheaper to refuse here; the
/// key is read once, when the binary is built.
pub(crate) fn default_release_key(value: Option<&str>) -> Result<Option<&str>, String> {
    let key = value.unwrap_or_default().trim();
    if key.is_empty() {
        return Ok(None);
    }
    let mut fields = key.split_whitespace();
    let kind = fields.next().unwrap_or_default();
    let body = fields.next().unwrap_or_default();
    let known = kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-");
    let base64 = body.len() >= 16
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
    if key.lines().count() != 1 || !known || !base64 {
        return Err(format!(
            "ACS_DEFAULT_RELEASE_KEY must be one ssh public key line, as in \
             `ssh-ed25519 AAAA... you@example.com` — the contents of a .pub file \
             (docs/VERSIONING.md, \"Forking\"); got {key:?}"
        ));
    }
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use super::default_release_key;

    /// A well-formed ed25519 public key line, as a `.pub` file holds it.
    const GOOD: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork";

    /// The rejection a build dies with, or a panic naming what was taken.
    fn rejected(value: &str) -> String {
        match default_release_key(Some(value)) {
            Err(why) => why,
            Ok(taken) => panic!("{value:?} was accepted as {taken:?}"),
        }
    }

    /// Unset and blank are the same thing and are not an error: the build
    /// keeps acs's own key rather than ending up with none.
    #[test]
    fn nothing_leaves_acss_own_key_in_place() {
        assert_eq!(default_release_key(None), Ok(None));
        for blank in ["", " ", "   \t ", "\n", "\n\n"] {
            assert_eq!(default_release_key(Some(blank)), Ok(None), "{blank:?}");
        }
    }

    /// One key line is taken as given, whatever surrounds it: `$(cat
    /// release_key.pub)` and a file read whole both arrive here.
    #[test]
    fn one_well_formed_key_is_taken_as_given() {
        assert_eq!(default_release_key(Some(GOOD)), Ok(Some(GOOD)));
        // A trailing newline is what a `.pub` file ends with, and leading
        // space is what a copy-paste adds; neither is a second line.
        assert_eq!(
            default_release_key(Some(&format!("{GOOD}\n"))),
            Ok(Some(GOOD))
        );
        assert_eq!(
            default_release_key(Some(&format!("  {GOOD}  "))),
            Ok(Some(GOOD))
        );
        // A comment is optional, and every key type ssh-keygen writes.
        for key in [
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX",
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDb+hd7b0K3+WQ5xHc me@example.com",
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTY= me@example.com",
            "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29t me@example.com",
        ] {
            assert_eq!(default_release_key(Some(key)), Ok(Some(key)), "{key}");
        }
    }

    /// A value that is not an ssh public key line fails the build rather
    /// than producing a binary that refuses every release.
    #[test]
    fn a_key_of_no_known_type_is_refused() {
        for bad in [
            "not-a-key",
            "not-a-key AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX",
            "rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDb+hd7b0K3+WQ5xHc me@example.com",
            // The private half, pasted by mistake: its first line is not a
            // key line at all.
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            let why = rejected(bad);
            assert!(why.contains("ACS_DEFAULT_RELEASE_KEY"), "{bad:?}: {why}");
            assert!(why.contains("one ssh public key line"), "{bad:?}: {why}");
        }
    }

    /// A key type with nothing usable after it: a truncated paste, or a
    /// body that is not base64 at all.
    #[test]
    fn a_body_too_short_or_not_base64_is_refused() {
        for bad in [
            "ssh-ed25519",
            "ssh-ed25519 AAAA fork",
            "ssh-ed25519 AAAAC3NzaC1lZD",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA!!!!AAAA fork",
            "ssh-ed25519 <paste the key here> fork",
        ] {
            let why = rejected(bad);
            assert!(why.contains("ACS_DEFAULT_RELEASE_KEY"), "{bad:?}: {why}");
        }
    }

    /// The load-bearing one: a second line would be a second allowed
    /// signer, which nobody means to write, and would corrupt the
    /// `cargo:rustc-env=` directive as well. Each line here is a key the
    /// check accepts on its own, so only the line count rejects it.
    #[test]
    fn more_than_one_line_is_refused() {
        let second =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILbxQW5C9X7CdwcQ4bab0gsQi4Evk2xfgmI/972dlHCb two";
        for bad in [
            format!("{GOOD}\n{second}"),
            format!("{GOOD}\n{second}\n"),
            format!("{GOOD}\n\n{second}\n"),
            // An authorized_keys file, or a `.pub` with something appended.
            format!("{GOOD}\n# a comment\n"),
        ] {
            let why = rejected(&bad);
            assert!(why.contains("ACS_DEFAULT_RELEASE_KEY"), "{bad:?}: {why}");
            assert_eq!(default_release_key(Some(GOOD)), Ok(Some(GOOD)));
        }
    }
}
