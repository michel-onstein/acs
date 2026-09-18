//! Record the target triple (the remote installer needs to know which build
//! this binary is) and point the `embed-payloads` feature at its data.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=ACS_TARGET={target}");
    println!("cargo:rerun-if-env-changed=ACS_PAYLOADS_FILE");
    if std::env::var_os("CARGO_FEATURE_EMBED_PAYLOADS").is_some() {
        let path = std::env::var("ACS_PAYLOADS_FILE").expect(
            "the embed-payloads feature needs ACS_PAYLOADS_FILE (cargo xtask dist sets it)",
        );
        println!("cargo:rerun-if-changed={path}");
        println!("cargo:rustc-env=ACS_PAYLOADS_FILE={path}");
    }
}
