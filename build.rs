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
    // the build rather than producing a binary that refuses every release.
    // What the variable means is `default_release_key`, in the file
    // included below so the tests can reach it; all that is left here is
    // the wiring. This is not the runtime `ACS_RELEASE_KEY`, which keeps
    // its guards (acs-o9v).
    println!("cargo:rerun-if-env-changed=ACS_DEFAULT_RELEASE_KEY");
    println!("cargo:rerun-if-changed=src/release_key.rs");
    let set = std::env::var("ACS_DEFAULT_RELEASE_KEY").ok();
    match default_release_key(set.as_deref()) {
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

// `default_release_key`, the one piece of judgement in this file. It is a
// source include rather than a function here because a build script is not
// part of the crate and so is not a test target: the crate compiles the
// same file (`#[cfg(test)] mod release_key`) to test it (acs-okz). Its own
// `#[cfg(test)]` tests are not compiled into this build script.
include!("src/release_key.rs");
