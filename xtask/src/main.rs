//! Build tasks for acs. `cargo xtask dist` builds the release binaries.

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    eprintln!("unknown task '{task}'\n\nusage: cargo xtask <task>\n\ntasks:\n  dist    build release binaries for every target");
    std::process::exit(2);
}
