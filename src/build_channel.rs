// What `ACS_DEFAULT_RELEASES_URL` and `ACS_DEFAULT_RELEASE_KEY` mean for a
// build — the release channel a fork bakes in (docs/VERSIONING.md,
// "Forking"). One file, compiled three times, so the rule cannot drift
// between the places that read the two variables:
//
// - `build.rs` `include!`s it, a build script being no part of the crate;
// - xtask compiles it (`#[path]`), for the channel `cargo xtask dist`
//   stamps on what it builds (`xtask/src/channel.rs`);
// - the crate compiles it under `cfg(test)` to test it. Build scripts are
//   not a test target, so a check that lives only in `build.rs` is
//   exercised only by whoever runs the build by hand; put here, it runs on
//   every `cargo test` (acs-okz).
//
// Nothing in here may name a crate or pull anything in: neither the build
// script nor xtask shares the others' dependencies. None of it reaches the
// `acs` binary.

/// What `ACS_DEFAULT_RELEASES_URL` tells this build to bake in: `None` when
/// it is unset or blank — the usual case, which leaves
/// `release::UPSTREAM_RELEASES_URL` in place — `Some` with the URL, trailing
/// `/` stripped, otherwise; and `Err` with the message the build dies with
/// when the value is not an https URL with a host (acs-d7x).
///
/// Not a URL validator: just enough that the realistic slips — `http://`,
/// a missing scheme, an empty or mistyped host, a stray space or line
/// break, a query that the paths appended to it would cut off — fail the
/// build rather than shipping a binary whose update channel is silently
/// unreachable, found only at a user's `acs upgrade`. The runtime
/// `ACS_RELEASES_URL` refuses anything but https as well (acs-95w), so a
/// build-time `http://` is almost certainly a mistake.
pub(crate) fn check_releases_url(value: Option<&str>) -> Result<Option<&str>, String> {
    let url = value.unwrap_or_default().trim();
    if url.is_empty() {
        return Ok(None);
    }
    match url_problem(url) {
        None => Ok(Some(url.trim_end_matches('/'))),
        Some(why) => Err(format!(
            "ACS_DEFAULT_RELEASES_URL must be one https URL with a host, as in \
             `https://github.com/you/acs/releases` — where the releases are published \
             (docs/VERSIONING.md, \"Forking\"); {why}: {url:?}"
        )),
    }
}

/// Why `url` is not one the build can bake in, or `None` when it is.
fn url_problem(url: &str) -> Option<String> {
    if !url.bytes().all(|b| b.is_ascii_graphic()) {
        return Some("it holds a space, a line break or a character no URL can".into());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Some("it has no scheme".into());
    };
    if scheme != "https" {
        return Some(format!("its scheme is {scheme}, not https"));
    }
    if rest.contains(['?', '#']) {
        return Some("a query or fragment would be cut off by the paths acs appends".into());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Some("it carries credentials, which every binary would then publish".into());
    }
    // `[v6]:port`, or `host:port`, the port optional either way.
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, after))
                if !h.is_empty()
                    && h.bytes()
                        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.') =>
            {
                match after {
                    "" => (h, None),
                    _ => match after.strip_prefix(':') {
                        Some(p) => (h, Some(p)),
                        None => return Some("its host is not a host name or address".into()),
                    },
                }
            }
            _ => return Some("its host is not a host name or address".into()),
        },
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    if host.is_empty() {
        return Some("its host is empty".into());
    }
    if !authority.starts_with('[') {
        let label_ok = |l: &str| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if !host.split('.').all(label_ok) {
            return Some(format!("its host {host:?} is not a host name or address"));
        }
    }
    if let Some(p) = port {
        if !matches!(p.parse::<u16>(), Ok(n) if n > 0) || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Some(format!("its port {p:?} is not a port"));
        }
    }
    None
}

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
///
/// The body is decoded, not just looked at (acs-d7x): it must be base64
/// whose blob names the same key type the line declares, and — for a plain
/// key — be made of whole length-prefixed fields, which is what a
/// truncated or mismatched paste is not. What it cannot catch is a
/// well-formed key that is simply the wrong one; that fails closed.
pub(crate) fn check_release_key(value: Option<&str>) -> Result<Option<&str>, String> {
    let key = value.unwrap_or_default().trim();
    if key.is_empty() {
        return Ok(None);
    }
    match key_problem(key) {
        None => Ok(Some(key)),
        Some(why) => Err(format!(
            "ACS_DEFAULT_RELEASE_KEY must be one ssh public key line, as in \
             `ssh-ed25519 AAAA... you@example.com` — the contents of a .pub file \
             (docs/VERSIONING.md, \"Forking\"); {why}: {key:?}"
        )),
    }
}

/// Why `key` is not one ssh public key line, or `None` when it is.
fn key_problem(key: &str) -> Option<String> {
    if key.lines().count() != 1 {
        return Some("it is more than one line".into());
    }
    let mut fields = key.split_whitespace();
    let kind = fields.next().unwrap_or_default();
    let body = fields.next().unwrap_or_default();
    if !(kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-")) {
        return Some(format!("{kind:?} is not an ssh key type"));
    }
    let Some(blob) = base64_decode(body) else {
        return Some("its body is not base64, or is cut short".into());
    };
    let Some((embedded, rest)) = ssh_string(&blob) else {
        return Some("its body is cut short".into());
    };
    if embedded != kind.as_bytes() {
        return Some(format!(
            "its body is a {:?} key, not the {kind} the line declares",
            String::from_utf8_lossy(embedded)
        ));
    }
    // A plain key's blob is nothing but length-prefixed fields; a
    // certificate's carries fixed-width integers too, so only its type is
    // checked.
    if !kind.contains("-cert-") {
        let mut rest = rest;
        if rest.is_empty() {
            return Some("its body holds a key type and no key".into());
        }
        while !rest.is_empty() {
            match ssh_string(rest) {
                Some((_, more)) => rest = more,
                None => return Some("its body is cut short".into()),
            }
        }
    }
    None
}

/// The first length-prefixed field of an ssh wire-format blob (RFC 4251
/// §5, `string`), and what follows it.
fn ssh_string(blob: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = blob.split_at_checked(4)?;
    let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    rest.split_at_checked(len)
}

/// Standard, padded base64 (RFC 4648 §4), strictly: a length that is a
/// multiple of four, padding only at the end, and no stray bits.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    // `& 3` rather than `is_multiple_of(4)`, which is newer than the MSRV.
    if text.is_empty() || text.len() & 3 != 0 {
        return None;
    }
    let pad = text.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in &text[..text.len() - pad] {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = ((acc << 6) | u32::from(v)) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (acc & ((1 << bits) - 1) == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, check_release_key, check_releases_url};

    /// A well-formed ed25519 public key line, as a `.pub` file holds it.
    const GOOD: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork";

    /// The rejection a build dies with, or a panic naming what was taken.
    fn rejected(value: &str) -> String {
        match check_release_key(Some(value)) {
            Err(why) => why,
            Ok(taken) => panic!("{value:?} was accepted as {taken:?}"),
        }
    }

    /// Unset and blank are the same thing and are not an error: the build
    /// keeps acs's own key rather than ending up with none.
    #[test]
    fn nothing_leaves_acss_own_key_in_place() {
        assert_eq!(check_release_key(None), Ok(None));
        for blank in ["", " ", "   \t ", "\n", "\n\n"] {
            assert_eq!(check_release_key(Some(blank)), Ok(None), "{blank:?}");
        }
    }

    /// One key line is taken as given, whatever surrounds it: `$(cat
    /// release_key.pub)` and a file read whole both arrive here.
    #[test]
    fn one_well_formed_key_is_taken_as_given() {
        assert_eq!(check_release_key(Some(GOOD)), Ok(Some(GOOD)));
        // A trailing newline is what a `.pub` file ends with, and leading
        // space is what a copy-paste adds; neither is a second line.
        assert_eq!(
            check_release_key(Some(&format!("{GOOD}\n"))),
            Ok(Some(GOOD))
        );
        assert_eq!(
            check_release_key(Some(&format!("  {GOOD}  "))),
            Ok(Some(GOOD))
        );
        // A comment is optional, and every key type ssh-keygen writes —
        // real keys from it, bar the sk- one, which needs a security key to
        // make and is built by hand to the same layout.
        for key in [
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX",
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDZ5SRdxFRAFlrlXHt+llviyUYOl0p6oOVxBcPPb/MkqGMAxZl1q9pamESy+4x4vWaM5Qqagl3ldtJaP9jSe1eX7dWcPuDyvYmIyXNRvJY1pGNittimpepL3Z6MLxL86yP8eeZ4dW3DwpzFpparTnfQVcdY+24V9EjbOdJCGH/u2w== me@example.com",
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFEtKdZICXkaJv9u5C49+YbOXnN8UtgCmMtOUZj2o3noOY7kW8Cr7pcECmHeyFw5n8ryTPE9tNBtAGLocUAUYI8= me@example.com",
            "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fAAAABHNzaDo= me@example.com",
        ] {
            assert_eq!(check_release_key(Some(key)), Ok(Some(key)), "{key}");
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
            assert!(why.contains("not an ssh key type"), "{bad:?}: {why}");
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
            // Padding in the middle, and three of it.
            "ssh-ed25519 AAAAC3Nz=C1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5A===",
        ] {
            let why = rejected(bad);
            assert!(why.contains("ACS_DEFAULT_RELEASE_KEY"), "{bad:?}: {why}");
            assert!(why.contains("cut short"), "{bad:?}: {why}");
        }
    }

    /// acs-d7x: a body that is base64 of the right shape but not a key of
    /// the declared type — every one of these passed the shape-only check.
    #[test]
    fn a_body_that_does_not_decode_to_the_declared_key_is_refused() {
        for (bad, says) in [
            // Shape-only passed sixteen As: twelve zero bytes, an empty type.
            ("ssh-ed25519 AAAAAAAAAAAAAAAA", "is a \"\" key"),
            // An ed25519 body under another type's name.
            (
                "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork",
                "is a \"ssh-ed25519\" key, not the ssh-rsa",
            ),
            // Truncated at a four-character boundary, so it still decodes:
            // the key field claims 32 bytes and holds 29.
            (
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4 fork",
                "cut short",
            ),
            // A type and no key at all.
            ("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5", "no key"),
            // Non-zero bits after the last byte, under the padding: not what
            // any encoder writes (the real key ends `u2w==`).
            (
                "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDZ5SRdxFRAFlrlXHt+llviyUYOl0p6oOVxBcPPb/MkqGMAxZl1q9pamESy+4x4vWaM5Qqagl3ldtJaP9jSe1eX7dWcPuDyvYmIyXNRvJY1pGNittimpepL3Z6MLxL86yP8eeZ4dW3DwpzFpparTnfQVcdY+24V9EjbOdJCGH/u2x==",
                "not base64",
            ),
        ] {
            let why = rejected(bad);
            assert!(why.contains("ACS_DEFAULT_RELEASE_KEY"), "{bad:?}: {why}");
            assert!(why.contains(says), "{bad:?}: {why}");
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
        assert_eq!(check_release_key(Some(second)), Ok(Some(second)));
        for bad in [
            format!("{GOOD}\n{second}"),
            format!("{GOOD}\n{second}\n"),
            format!("{GOOD}\n\n{second}\n"),
            // An authorized_keys file, or a `.pub` with something appended.
            format!("{GOOD}\n# a comment\n"),
        ] {
            let why = rejected(&bad);
            assert!(why.contains("more than one line"), "{bad:?}: {why}");
        }
    }

    #[test]
    fn base64_decodes_as_rfc_4648_says() {
        for (text, bytes) in [
            ("Zg==", &b"f"[..]),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
            ("+/+/", &[0xfb, 0xff, 0xbf]),
        ] {
            assert_eq!(base64_decode(text).as_deref(), Some(bytes), "{text}");
        }
        for bad in ["", "Zg", "Zg=", "Zh==", "Z===", "Zm9v\n", "Zm 9"] {
            assert_eq!(base64_decode(bad), None, "{bad:?}");
        }
    }

    /// The rejection a build dies with for a releases URL.
    fn url_rejected(value: &str) -> String {
        match check_releases_url(Some(value)) {
            Err(why) => why,
            Ok(taken) => panic!("{value:?} was accepted as {taken:?}"),
        }
    }

    /// Unset and blank leave acs's own releases in place.
    #[test]
    fn no_releases_url_leaves_acss_own_in_place() {
        assert_eq!(check_releases_url(None), Ok(None));
        for blank in ["", " ", "\t", "\n"] {
            assert_eq!(check_releases_url(Some(blank)), Ok(None), "{blank:?}");
        }
    }

    /// An https URL with a host is taken as given, trimmed and without a
    /// trailing `/`.
    #[test]
    fn an_https_releases_url_is_taken_as_given() {
        for (set, baked) in [
            (
                "https://github.com/you/acs/releases",
                "https://github.com/you/acs/releases",
            ),
            (
                " https://github.com/you/acs/releases/\n",
                "https://github.com/you/acs/releases",
            ),
            ("https://dl.example.com", "https://dl.example.com"),
            ("https://dl.example.com/", "https://dl.example.com"),
            (
                "https://mirror-1.example.co.uk:8443/acs",
                "https://mirror-1.example.co.uk:8443/acs",
            ),
            ("https://10.0.0.1/acs", "https://10.0.0.1/acs"),
            (
                "https://[2001:db8::1]:443/acs",
                "https://[2001:db8::1]:443/acs",
            ),
            ("https://localhost/acs", "https://localhost/acs"),
        ] {
            assert_eq!(check_releases_url(Some(set)), Ok(Some(baked)), "{set:?}");
        }
    }

    /// acs-d7x: the realistic slips fail the build, with the same shape of
    /// message as a bad key, rather than shipping an unreachable channel.
    #[test]
    fn a_mistyped_releases_url_is_refused() {
        for (bad, says) in [
            // Not https: the runtime override would refuse these too.
            (
                "http://github.com/you/acs/releases",
                "scheme is http, not https",
            ),
            ("file:///srv/acs/releases", "scheme is file, not https"),
            ("HTTPS://github.com/you/acs/releases", "scheme is HTTPS"),
            // No scheme at all, or a mangled one.
            ("github.com/you/acs/releases", "no scheme"),
            ("https:/github.com/you/acs/releases", "no scheme"),
            ("https//github.com/you/acs/releases", "no scheme"),
            // An empty host.
            ("https://", "host is empty"),
            ("https:///you/acs/releases", "host is empty"),
            ("https://:443/acs", "host is empty"),
            // A host that is not one.
            ("https://github..com/you/acs", "not a host name"),
            ("https://github_com/you/acs", "not a host name"),
            ("https://-github.com/acs", "not a host name"),
            ("https://[2001:db8::1/acs", "not a host name"),
            ("https://[]/acs", "not a host name"),
            ("https://[::1]x/acs", "not a host name"),
            // A port that is not one.
            ("https://github.com:/acs", "is not a port"),
            ("https://github.com:http/acs", "is not a port"),
            ("https://github.com:99999/acs", "is not a port"),
            ("https://github.com:0/acs", "is not a port"),
            ("https://github.com:+443/acs", "is not a port"),
            // Unparseable: whitespace, a line break, a non-ASCII character.
            (
                "https://github.com/you/acs releases",
                "a space, a line break",
            ),
            (
                "https://github.com/you/acs\nhttps://x",
                "a space, a line break",
            ),
            ("https://gíthub.com/you/acs", "a space, a line break"),
            // Something every appended path would land inside.
            (
                "https://github.com/you/acs?tab=releases",
                "query or fragment",
            ),
            ("https://github.com/you/acs#releases", "query or fragment"),
            // Credentials baked into every binary.
            ("https://me:secret@github.com/you/acs", "credentials"),
        ] {
            let why = url_rejected(bad);
            assert!(why.contains("ACS_DEFAULT_RELEASES_URL"), "{bad:?}: {why}");
            assert!(why.contains("one https URL with a host"), "{bad:?}: {why}");
            assert!(why.contains(says), "{bad:?}: {why}");
        }
    }
}
