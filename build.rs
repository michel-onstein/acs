//! Record the target triple (the remote installer needs to know which build
//! this binary is), let a fork bake in its own releases URL, and point the
//! `embed-payloads` feature at its data.

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
    println!("cargo:rerun-if-env-changed=ACS_PAYLOADS_FILE");
    if std::env::var_os("CARGO_FEATURE_EMBED_PAYLOADS").is_some() {
        let path = std::env::var("ACS_PAYLOADS_FILE").expect(
            "the embed-payloads feature needs ACS_PAYLOADS_FILE (cargo xtask dist sets it)",
        );
        println!("cargo:rerun-if-changed={path}");
        println!("cargo:rustc-env=ACS_PAYLOADS_FILE={path}");
    }
}
