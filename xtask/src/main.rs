//! Build tasks for acs.
//!
//! `cargo xtask dist [--targets a,b] [--out dir]` builds release binaries for
//! every target (DESIGN §8.1, §9):
//!
//! 1. slim builds — Linux targets with `cargo zigbuild` (static musl), macOS
//!    targets with `cargo build` (on a macOS host only);
//! 2. the payload blob: the Linux slim builds, gzip'd;
//! 3. complete Linux builds: slim + appended blob;
//! 4. complete macOS builds: rebuilt with `--features embed-payloads`, then
//!    ad-hoc signed;
//! 5. a size report, failing over budget (slim 1.1 MB, complete 2.1 MB);
//! 6. `source.stamp`, the fingerprint of the sources it built from
//!    (`scripts/source_stamp.sh`), which `scripts/e2e_ssh.sh --no-build`
//!    checks before it reuses the binaries (acs-gb4), and `channel.stamp`,
//!    the releases URL and release key they were built for, which
//!    `cargo xtask package` checks before it packages them (acs-u3qn).
//!
//! `--targets` takes only the targets acs releases ([`LINUX`], [`MAC`]): a
//! typo is refused up front rather than leaving a `dist/` one platform
//! short (acs-u3qn).
//!
//! `cargo xtask bump` releases the next version, `cargo xtask package` makes
//! the release assets and `cargo xtask formula` the Homebrew formula (see
//! `bump.rs`, `package.rs`, `formula.rs` and docs/VERSIONING.md).

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

mod bump;
mod channel;
mod formula;
mod package;
mod upstream;

const LINUX: &[&str] = &["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];
const MAC: &[&str] = &["aarch64-apple-darwin", "x86_64-apple-darwin"];
const SLIM_BUDGET: u64 = 1_100_000;
const COMPLETE_BUDGET: u64 = 2_100_000;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dist") => dist(&args[1..]),
        Some("bump") => {
            if let Err(e) = bump::main(&args[1..]) {
                eprintln!("xtask bump: {e}");
                exit(1);
            }
        }
        Some("formula") => {
            if let Err(e) = formula::main(&args[1..]) {
                eprintln!("xtask formula: {e}");
                exit(1);
            }
        }
        Some("package") => {
            if let Err(e) = package::main(&args[1..]) {
                eprintln!("xtask package: {e}");
                exit(1);
            }
        }
        // The release channel this xtask was built for, in the stamp's form:
        // the key `scripts/update-tap.sh vX.Y.Z` checks a published
        // SHA256SUMS.sig against, so a fork's tap trusts the fork's key with
        // no edit to the script (acs-oxl4).
        Some("channel") if args.len() == 1 => print!("{}", channel::Channel::built().stamp()),
        _ => {
            eprintln!(
                "usage: cargo xtask dist [--targets t1,t2] [--out dir]\n       cargo xtask bump [--dry-run] [--major|--minor|--patch] (see docs/VERSIONING.md)\n       cargo xtask package --dist DIR --version X.Y.Z --out DIR\n       cargo xtask formula --version X.Y.Z --sums SHA256SUMS [--out FILE]\n       cargo xtask channel"
            );
            exit(2);
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run(cmd: &mut Command) {
    eprintln!("+ {cmd:?}");
    let st = cmd.status().unwrap_or_else(|e| {
        eprintln!("xtask: cannot run {:?}: {e}", cmd.get_program());
        exit(1)
    });
    if !st.success() {
        eprintln!("xtask: {:?} failed ({st})", cmd.get_program());
        exit(1);
    }
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn build(target: &str, embed: Option<&Path>) -> PathBuf {
    let linux = target.contains("-linux-");
    let mut cmd = Command::new(cargo());
    cmd.current_dir(root());
    if linux {
        cmd.arg("zigbuild");
    } else {
        cmd.arg("build");
    }
    cmd.args(["--release", "-p", "acs", "--target", target]);
    if let Some(blob) = embed {
        cmd.args(["--features", "embed-payloads"]);
        cmd.env("ACS_PAYLOADS_FILE", blob);
    } else {
        cmd.env_remove("ACS_PAYLOADS_FILE");
    }
    run(&mut cmd);
    root()
        .join("target")
        .join(target)
        .join("release")
        .join("acs")
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new("gzip")
        .args(["-9", "-n", "-c"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("gzip");
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().expect("gzip output");
    writer.join().unwrap().expect("write to gzip");
    assert!(out.status.success(), "gzip failed");
    out.stdout
}

/// The fingerprint of the sources this build comes from, from
/// `scripts/source_stamp.sh`.
fn source_stamp() -> String {
    let script = root().join("scripts/source_stamp.sh");
    let out = Command::new(&script)
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("xtask: cannot run {}: {e}", script.display());
            exit(1)
        });
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        eprintln!("xtask: {} failed ({})", script.display(), out.status);
        exit(1);
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// A `--targets` list: comma-separated, each one a target acs releases,
/// none twice. Anything else is refused before a build starts — a
/// mistyped triple would otherwise build nothing for it and leave a
/// `dist/` silently a platform short (acs-u3qn).
fn parse_targets(list: &str) -> Result<Vec<String>, String> {
    let mut targets: Vec<String> = Vec::new();
    for t in list.split(',').map(str::trim) {
        if !LINUX.contains(&t) && !MAC.contains(&t) {
            return Err(format!(
                "--targets: {t:?} is not a target acs releases; pick from {}",
                [LINUX, MAC].concat().join(", ")
            ));
        }
        if targets.iter().any(|have| have == t) {
            return Err(format!("--targets: {t} is named twice"));
        }
        targets.push(t.to_string());
    }
    Ok(targets)
}

fn dist(args: &[String]) {
    let mut out = root().join("dist");
    let mut targets: Vec<String> = LINUX.iter().map(|s| s.to_string()).collect();
    if cfg!(target_os = "macos") {
        targets.extend(MAC.iter().map(|s| s.to_string()));
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let Some(value) = it.next() else {
            eprintln!("xtask dist: {a} needs a value");
            exit(2);
        };
        match a.as_str() {
            "--out" => out = PathBuf::from(value),
            "--targets" => {
                targets = parse_targets(value).unwrap_or_else(|why| {
                    eprintln!("xtask dist: {why}");
                    exit(2)
                })
            }
            other => {
                eprintln!("xtask dist: unknown argument {other}");
                exit(2);
            }
        }
    }

    // The channel these binaries are for. `build.rs` bakes in whatever the
    // environment says now; `package` will compare the stamp against what
    // *it* was compiled with. An xtask compiled for one channel and run in
    // another's environment (a cached binary) would record the wrong one,
    // so that is refused here (acs-u3qn).
    let channel = channel::Channel::built();
    let env = channel::Channel::from_env(
        std::env::var("ACS_DEFAULT_RELEASES_URL").ok().as_deref(),
        std::env::var("ACS_DEFAULT_RELEASE_KEY").ok().as_deref(),
    );
    if channel != env {
        eprintln!(
            "xtask dist: this xtask was built for releases {} (key {}), but the environment \
             builds for {} (key {}); run it through `cargo xtask` so it is rebuilt",
            channel.releases, channel.key, env.releases, env.key
        );
        exit(1);
    }
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    // 0. What we are about to build from, read before the first build so it
    //    describes what cargo actually reads. Written at the very end, so a
    //    build that fails leaves no stamp at all.
    let stamp = source_stamp();

    // 1. Slim builds.
    let mut slim = Vec::new();
    for t in &targets {
        let bin = build(t, None);
        let data = std::fs::read(&bin).unwrap_or_else(|e| {
            eprintln!("xtask: {}: {e}", bin.display());
            exit(1)
        });
        slim.push((t.clone(), data));
    }

    // 2. Payload blob of the Linux slim builds.
    let gz: Vec<(String, Vec<u8>, Vec<u8>)> = slim
        .iter()
        .filter(|(t, _)| t.contains("-linux-"))
        .map(|(t, d)| (t.clone(), d.clone(), gzip(d)))
        .collect();
    let inputs: Vec<acs::payload::Input<'_>> = gz
        .iter()
        .map(|(t, raw, g)| acs::payload::Input {
            target: t,
            raw,
            gz: g,
        })
        .collect();
    let blob = acs::payload::build(&inputs);
    let blob_path = out.join("payloads.bin");
    std::fs::write(&blob_path, &blob).unwrap();
    let blob_path = std::fs::canonicalize(&blob_path).unwrap();

    // 3./4. Complete builds.
    let mut report = Vec::new();
    let mut over = false;
    for (t, data) in &slim {
        let dir = out.join(t);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("acs");
        if t.contains("-linux-") {
            let mut full = data.clone();
            full.extend_from_slice(&blob);
            std::fs::write(&dest, &full).unwrap();
        } else {
            let bin = build(t, Some(&blob_path));
            std::fs::copy(&bin, &dest).unwrap();
            run(Command::new("codesign").args(["-s", "-", "-f"]).arg(&dest));
            run(Command::new("codesign").arg("-v").arg(&dest));
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        let slim_len = data.len() as u64;
        let full_len = size(&dest);
        over |= slim_len > SLIM_BUDGET || full_len > COMPLETE_BUDGET;
        report.push((t.clone(), slim_len, full_len));
    }

    // 5. Report.
    let gz_total: usize = gz.iter().map(|(_, _, g)| g.len()).sum();
    eprintln!();
    eprintln!("{:<32} {:>10} {:>10}", "target", "slim", "complete");
    for (t, s, f) in &report {
        eprintln!("{t:<32} {s:>10} {f:>10}");
    }
    eprintln!(
        "payload blob: {} bytes ({} gzip'd builds, {gz_total} bytes)",
        blob.len(),
        gz.len()
    );
    eprintln!("budget: slim ≤ {SLIM_BUDGET}, complete ≤ {COMPLETE_BUDGET}");
    eprintln!("output: {}", out.display());
    if over {
        eprintln!("xtask: over the size budget");
        exit(1);
    }

    // 6. The source stamp, beside the binaries it describes: it goes into the
    //    directory this run wiped and refilled, last, so it cannot outlive
    //    them and a run that failed leaves none. `scripts/e2e_ssh.sh
    //    --no-build` refuses to reuse a dist/ whose stamp is not this tree's
    //    (acs-gb4).
    std::fs::write(out.join("source.stamp"), format!("{stamp}\n")).unwrap();
    eprintln!("source stamp: {stamp}");
    // The channel, the same way: `cargo xtask package` refuses a dist/ that
    // was built for another one (acs-u3qn).
    std::fs::write(out.join(channel::FILE), channel.stamp()).unwrap();
    eprintln!("channel: releases {}", channel.releases);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_acs_releases_are_taken_in_order() {
        assert_eq!(
            parse_targets("aarch64-apple-darwin,x86_64-unknown-linux-musl").unwrap(),
            ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"]
        );
        let all = [LINUX, MAC].concat().join(",");
        assert_eq!(parse_targets(&all).unwrap().len(), 4);
    }

    /// acs-u3qn: a typo used to build nothing for that target and leave a
    /// dist/ one platform short.
    #[test]
    fn a_target_acs_does_not_release_is_refused() {
        for bad in [
            "x86_64-unknown-linux-mus",
            "x86_64-unknown-linux-musl,aarch64-apple-darwn",
            "x86_64-unknown-linux-gnu",
            "",
            "x86_64-unknown-linux-musl,",
        ] {
            let why = parse_targets(bad).unwrap_err();
            assert!(why.contains("not a target acs releases"), "{bad:?}: {why}");
        }
        let why = parse_targets("aarch64-apple-darwin,aarch64-apple-darwin").unwrap_err();
        assert!(why.contains("named twice"), "{why}");
    }
}
