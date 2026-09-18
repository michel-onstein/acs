//! `cargo xtask package`: turn `cargo xtask dist` output into release assets
//! (docs/VERSIONING.md, "Binaries"):
//!
//! - `acs-<version>-<target>.tar.gz` per target, holding
//!   `acs-<version>-<target>/acs` and the README;
//! - `SHA256SUMS` over the archives;
//! - `install.sh`, the one-line installer (`--installer`);
//! - `NOTES.md`: install instructions, checksums and the changes.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A short platform name for the install table.
pub fn platform(target: &str) -> &str {
    match target {
        "aarch64-apple-darwin" => "macOS, Apple silicon",
        "x86_64-apple-darwin" => "macOS, Intel",
        "x86_64-unknown-linux-musl" => "Linux x86_64 (static)",
        "aarch64-unknown-linux-musl" => "Linux aarch64 (static)",
        other => other,
    }
}

/// Release notes for `version` with the given archives and changes.
pub fn notes(version: &str, archives: &[(String, String)], changes: &str) -> String {
    let mut n = String::new();
    n.push_str(&format!(
        "acs {version} — persistent ssh sessions with an unfiltered terminal stream.\n\n"
    ));
    n.push_str("## Install\n\n");
    n.push_str("```sh\ncurl -fsSL https://github.com/michel-onstein/acs/releases/latest/download/install.sh | sh\n```\n\n");
    n.push_str(&format!(
        "installs the latest release for your machine into `~/.local/bin` (as root, `/usr/local/bin`), checked against `SHA256SUMS`. `curl … | ACS_VERSION={version} sh` installs this one.\n\n"
    ));
    n.push_str("With Homebrew: `brew install michel-onstein/acs/acs` (the tap is updated with each release).\n\n");
    n.push_str("Or pick the archive for your machine by hand; every build can install acs on Linux hosts (x86_64 and aarch64) by itself on first contact.\n\n");
    n.push_str("| Platform | Archive |\n| --- | --- |\n");
    for (target, file) in archives {
        n.push_str(&format!("| {} | `{file}` |\n", platform(target)));
    }
    let example = archives
        .iter()
        .map(|(_, f)| f.as_str())
        .find(|f| f.contains("aarch64-apple-darwin"))
        .or(archives.first().map(|(_, f)| f.as_str()))
        .unwrap_or("acs.tar.gz");
    let dir = example.trim_end_matches(".tar.gz");
    n.push_str(&format!(
        "\n```sh\ncurl -LO https://github.com/michel-onstein/acs/releases/download/v{version}/{example}\ntar xzf {example}\ninstall -m 755 {dir}/acs ~/.local/bin/acs\nacs --version\n```\n\n"
    ));
    n.push_str("On macOS, a file downloaded with a browser is quarantined; clear it with `xattr -d com.apple.quarantine ~/.local/bin/acs` (curl does not set it).\n\n");
    n.push_str(
        "Verify with `shasum -a 256 -c SHA256SUMS` (macOS) or `sha256sum -c SHA256SUMS` (Linux).\n",
    );
    if !changes.trim().is_empty() {
        n.push_str("\n## Changes\n\n");
        n.push_str(changes.trim_end());
        n.push('\n');
    }
    n
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let st = cmd
        .status()
        .map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({st})", cmd.get_program()))
    }
}

/// Build the assets from `dist` into `out`; returns the files written.
pub fn package(
    dist: &Path,
    version: &str,
    out: &Path,
    readme: Option<&Path>,
    installer: Option<&Path>,
    changes: &str,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut targets: Vec<String> = std::fs::read_dir(dist)
        .map_err(|e| format!("{}: {e}", dist.display()))?
        .flatten()
        .filter(|e| e.path().join("acs").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    targets.sort();
    if targets.is_empty() {
        return Err(format!("no <target>/acs under {}", dist.display()));
    }
    let stage = out.join(".stage");
    let _ = std::fs::remove_dir_all(&stage);
    let mut archives = Vec::new();
    let mut written = Vec::new();
    for t in &targets {
        let name = format!("acs-{version}-{t}");
        let dir = stage.join(&name);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::copy(dist.join(t).join("acs"), dir.join("acs")).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("acs"), std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        if let Some(r) = readme {
            std::fs::copy(r, dir.join("README.md")).map_err(|e| e.to_string())?;
        }
        let file = format!("{name}.tar.gz");
        let archive = out.join(&file);
        // COPYFILE_DISABLE keeps macOS tar from adding ._ resource files.
        run(Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg(&name))?;
        archives.push((t.clone(), file));
        written.push(archive);
    }
    let _ = std::fs::remove_dir_all(&stage);
    let mut sums = String::new();
    for (_, file) in &archives {
        let data = std::fs::read(out.join(file)).map_err(|e| e.to_string())?;
        sums.push_str(&format!(
            "{}  {file}\n",
            acs::sha256::hex(&acs::sha256::digest(&data))
        ));
    }
    std::fs::write(out.join("SHA256SUMS"), &sums).map_err(|e| e.to_string())?;
    written.push(out.join("SHA256SUMS"));
    if let Some(i) = installer {
        let dest = out.join("install.sh");
        std::fs::copy(i, &dest).map_err(|e| format!("{}: {e}", i.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        written.push(dest);
    }
    std::fs::write(out.join("NOTES.md"), notes(version, &archives, changes))
        .map_err(|e| e.to_string())?;
    written.push(out.join("NOTES.md"));
    Ok(written)
}

pub fn main(args: &[String]) -> Result<(), String> {
    let mut dist = None;
    let mut version = None;
    let mut out = None;
    let mut readme = None;
    let mut installer = None;
    let mut changes = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--dist" => dist = Some(PathBuf::from(val()?)),
            "--version" => version = Some(val()?.trim_start_matches('v').to_string()),
            "--out" => out = Some(PathBuf::from(val()?)),
            "--readme" => readme = Some(PathBuf::from(val()?)),
            "--installer" => installer = Some(PathBuf::from(val()?)),
            "--changes" => {
                let p = val()?;
                changes = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (dist, version, out) = match (dist, version, out) {
        (Some(d), Some(v), Some(o)) => (d, v, o),
        _ => return Err("usage: cargo xtask package --dist DIR --version X.Y.Z --out DIR [--readme FILE] [--installer FILE] [--changes FILE]".into()),
    };
    for f in package(
        &dist,
        &version,
        &out,
        readme.as_deref(),
        installer.as_deref(),
        &changes,
    )? {
        println!("{}", f.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_list_every_archive_and_the_changes() {
        let a = vec![
            (
                "aarch64-apple-darwin".to_string(),
                "acs-1.2.3-aarch64-apple-darwin.tar.gz".to_string(),
            ),
            (
                "x86_64-unknown-linux-musl".to_string(),
                "acs-1.2.3-x86_64-unknown-linux-musl.tar.gz".to_string(),
            ),
        ];
        let n = notes("1.2.3", &a, "- feat: something\n");
        assert!(n.contains("| macOS, Apple silicon | `acs-1.2.3-aarch64-apple-darwin.tar.gz` |"));
        assert!(n.contains("| Linux x86_64 (static) |"));
        assert!(n.contains("releases/download/v1.2.3/acs-1.2.3-aarch64-apple-darwin.tar.gz"));
        assert!(n.contains("install -m 755 acs-1.2.3-aarch64-apple-darwin/acs"));
        assert!(n.contains("## Changes\n\n- feat: something"));
        // The one-liner leads, with a way to pin this release.
        let install = n.find("releases/latest/download/install.sh | sh").unwrap();
        assert!(install < n.find("| Platform |").unwrap());
        assert!(n.contains("ACS_VERSION=1.2.3 sh"));
        assert!(n.contains("`brew install michel-onstein/acs/acs`"));
        assert!(!notes("1.2.3", &a, "  ").contains("## Changes"));
    }
}
