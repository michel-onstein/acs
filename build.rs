//! Record the target triple (the remote installer needs to know which build
//! this binary is), let a fork bake in its own releases URL and release
//! signing key, and point the `embed-payloads` feature at its data.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=ACS_TARGET={target}");
    // Where this build's releases come from, for a fork that publishes its
    // own (docs/VERSIONING.md, "Forking"). Unset — the usual case — leaves
    // `release::DEFAULT_RELEASES_URL` at acs's own releases, and blank
    // counts as unset. A value that is not an https URL with a host fails
    // the build rather than producing a binary whose update channel is
    // silently unreachable (acs-d7x). What the variable means is
    // `check_releases_url`, in the file included below. This is not the
    // runtime `ACS_RELEASES_URL`, which keeps its guards (acs-95w).
    println!("cargo:rerun-if-env-changed=ACS_DEFAULT_RELEASES_URL");
    println!("cargo:rerun-if-changed=src/build_channel.rs");
    let set = std::env::var("ACS_DEFAULT_RELEASES_URL").ok();
    match check_releases_url(set.as_deref()) {
        Ok(Some(url)) => println!("cargo:rustc-env=ACS_DEFAULT_RELEASES_URL={url}"),
        Ok(None) => {}
        Err(why) => panic!("{why}"),
    }
    // The key this build checks a release's SHA256SUMS signature against,
    // for a fork that signs its own releases (docs/VERSIONING.md,
    // "Forking"). It travels with the URL above: `signature::key_for` uses
    // the built-in key for whatever `DEFAULT_RELEASES_URL` names. Unset —
    // the usual case — leaves `signature::RELEASE_KEY` at acs's own key,
    // and blank counts as unset, so neither can leave a build with nothing
    // to verify against. A value that is not an ssh public key line fails
    // the build rather than producing a binary that refuses every release.
    // What the variable means is `check_release_key`, in the file included
    // below so the tests can reach it; all that is left here is the
    // wiring. This is not the runtime `ACS_RELEASE_KEY`, which keeps
    // its guards (acs-o9v).
    println!("cargo:rerun-if-env-changed=ACS_DEFAULT_RELEASE_KEY");
    let set = std::env::var("ACS_DEFAULT_RELEASE_KEY").ok();
    match check_release_key(set.as_deref()) {
        Ok(Some(key)) => println!("cargo:rustc-env=ACS_DEFAULT_RELEASE_KEY={key}"),
        Ok(None) => {}
        Err(why) => panic!("{why}"),
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

// `check_releases_url` and `check_release_key`, the only judgement in this
// file. They are a source include rather than functions here because a
// build script is not part of the crate and so is not a test target: the
// crate compiles the same file (`#[cfg(test)] mod build_channel`) to test
// it (acs-okz), and xtask compiles it for the channel it stamps. Its own
// `#[cfg(test)]` tests are not compiled into this build script.
include!("src/build_channel.rs");
