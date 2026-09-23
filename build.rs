//! Record the target triple (the remote installer needs to know which build
//! this binary is), let a fork bake in its own releases URL and release
//! signing key, and point the `embed-payloads` feature at its data.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=ACS_TARGET={target}");
    // Where this build's releases come from, for a fork that publishes its
    // own (docs/VERSIONING.md, "Forking"). Unset — the usual case — leaves
    // `release::DEFAULT_RELEASES_URL` at acs's own releases. Blank counts as
    // unset; the value is taken as given otherwise, since whoever builds the
    // binary already decides what it does. This is not the runtime
    // `ACS_RELEASES_URL`, which keeps its guards (acs-95w).
    println!("cargo:rerun-if-env-changed=ACS_DEFAULT_RELEASES_URL");
    if let Ok(url) = std::env::var("ACS_DEFAULT_RELEASES_URL") {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() {
            println!("cargo:rustc-env=ACS_DEFAULT_RELEASES_URL={url}");
        }
    }
    // The key this build checks a release's SHA256SUMS signature against,
    // for a fork that signs its own releases (docs/VERSIONING.md,
    // "Forking"). It travels with the URL above: `signature::key_for` uses
    // the built-in key for whatever `DEFAULT_RELEASES_URL` names. Unset —
    // the usual case — leaves `signature::RELEASE_KEY` at acs's own key,
    // and blank counts as unset, so neither can leave a build with nothing
    // to verify against. A value that is not an ssh public key line fails
    // the build rather than producing a binary that refuses every release
    // (`check_release_key`). This is not the runtime `ACS_RELEASE_KEY`,
    // which keeps its guards (acs-o9v).
    println!("cargo:rerun-if-env-changed=ACS_DEFAULT_RELEASE_KEY");
    if let Ok(key) = std::env::var("ACS_DEFAULT_RELEASE_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            check_release_key(key);
            println!("cargo:rustc-env=ACS_DEFAULT_RELEASE_KEY={key}");
        }
    }
    println!("cargo:rerun-if-env-changed=ACS_PAYLOADS_FILE");
    if std::env::var_os("CARGO_FEATURE_EMBED_PAYLOADS").is_some() {
        let path = std::env::var("ACS_PAYLOADS_FILE").expect(
            "the embed-payloads feature needs ACS_PAYLOADS_FILE (cargo xtask dist sets it)",
        );
        println!("cargo:rerun-if-changed={path}");
        println!("cargo:rustc-env=ACS_PAYLOADS_FILE={path}");
    }
}

/// Stop the build unless `key` is one ssh public key line — a known key
/// type and a base64 body, as `ssh-keygen` writes into a `.pub` file.
///
/// The key *is* the check (`src/signature.rs`): a mistyped one does not
/// weaken verification — every release then fails to verify, which is the
/// safe direction — but it is a whole release nobody can install, found by
/// a user rather than by the person who built it. A second line would be a
/// second allowed signer, which nobody means to write. Cheaper to refuse
/// here; the key is read once, when the binary is built.
fn check_release_key(key: &str) {
    let mut fields = key.split_whitespace();
    let kind = fields.next().unwrap_or_default();
    let body = fields.next().unwrap_or_default();
    let known = kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-");
    let base64 = body.len() >= 16
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
    if key.lines().count() != 1 || !known || !base64 {
        panic!(
            "ACS_DEFAULT_RELEASE_KEY must be one ssh public key line, as in \
             `ssh-ed25519 AAAA... you@example.com` — the contents of a .pub file \
             (docs/VERSIONING.md, \"Forking\"); got {key:?}"
        );
    }
}
