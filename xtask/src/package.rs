//! `cargo xtask package`: turn `cargo xtask dist` output into release assets
//! (docs/VERSIONING.md, "Binaries"):
//!
//! - `acs-<version>-<target>.tar.gz` per target, holding
//!   `acs-<version>-<target>/acs` and the README;
//! - `SHA256SUMS` over the archives;
//! - `install.sh`, the one-line installer (`--installer`), carrying this
//!   build's releases URL and release key (acs-x57);
//! - `NOTES.md`: install instructions, checksums and the changes.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::upstream;

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

/// Release notes for `version` with the given archives and changes, for
/// wherever this build publishes (acs-x57).
pub fn notes(version: &str, archives: &[(String, String)], changes: &str) -> String {
    notes_with(
        version,
        archives,
        changes,
        upstream::releases(),
        upstream::brew_ref(upstream::tap_from_env().as_deref()).as_deref(),
    )
}

/// [`notes`] for a given releases URL and `brew install` argument, so a
/// fork's notes can be tested without rebuilding acs.
pub fn notes_with(
    version: &str,
    archives: &[(String, String)],
    changes: &str,
    releases: &str,
    brew: Option<&str>,
) -> String {
    let mut n = String::new();
    n.push_str(&format!(
        "acs {version} — persistent ssh sessions with an unfiltered terminal stream.\n\n"
    ));
    n.push_str("## Install\n\n");
    n.push_str(&format!(
        "```sh\ncurl -fsSL {releases}/latest/download/install.sh | sh\n```\n\n"
    ));
    n.push_str(&format!(
        "installs the latest release for your machine into `~/.local/bin` (as root, `/usr/local/bin`), checked against `SHA256SUMS`. `curl … | ACS_VERSION={version} sh` installs this one.\n\n"
    ));
    if let Some(brew) = brew {
        n.push_str(&format!(
            "With Homebrew: `brew install {brew}` (the tap is updated with each release).\n\n"
        ));
    }
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
        "\n```sh\ncurl -LO {releases}/download/v{version}/{example}\ntar xzf {example}\ninstall -m 755 {dir}/acs ~/.local/bin/acs\nacs --version\n```\n\n"
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

/// `value` as one POSIX shell word: single quotes, with an embedded quote
/// written the only way a single-quoted string can hold one.
///
/// Inside single quotes `sh` expands nothing at all — no `$`, no backtick,
/// no `\`, and a newline is a newline rather than the end of a command — so
/// this is total: whatever the build was given becomes exactly that string
/// and can be nothing else. That is the whole of the injection answer for
/// [`install_script`], and it is why neither value needs to be validated
/// *for safety* before it goes in. Whether a build-time releases URL is a
/// sensible URL at all is a separate question, and a separate bead
/// (acs-d7x); the key already has to be one ssh public key line
/// (`src/release_key.rs`).
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `scripts/install.sh` with this release's releases URL and release key
/// substituted in, for the copy that ships as a release asset (acs-x57).
///
/// The script is otherwise byte for byte the one in the repository. Each
/// value has exactly one assignment, at the start of a line of its own; if
/// the script drifts so that one of them is missing, or has come to be
/// written twice, packaging **fails** — a fork whose installer silently
/// kept acs's key and acs's releases would send its users upstream, which
/// is worse than a release that does not build.
pub fn install_script(script: &str, releases: &str, key: &str) -> Result<String, String> {
    let mut lines: Vec<String> = script.lines().map(String::from).collect();
    for (name, value) in [("default_releases", releases), ("release_key", key)] {
        // Quoting makes a newline harmless — it would be part of the
        // string, not the end of the command — but it would also leave the
        // installer with an assignment spanning two lines, which nothing
        // could read back to check it against the binary. Neither value is
        // one that ever holds a newline.
        if value.contains(['\n', '\r']) {
            return Err(format!(
                "this build's {name} has a line break in it, so it cannot be one line \
                 of the installer: {value:?}"
            ));
        }
        let prefix = format!("{name}=");
        let at: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with(&prefix))
            .map(|(i, _)| i)
            .collect();
        match at[..] {
            [i] => lines[i] = format!("{name}={}", sh_quote(value)),
            [] => {
                return Err(format!(
                    "the installer has no `{name}=` line to put this build's value in \
                     (scripts/install.sh, acs-x57)"
                ))
            }
            ref many => {
                return Err(format!(
                    "the installer assigns {name} on {} lines; it must be one \
                     (scripts/install.sh, acs-x57)",
                    many.len()
                ))
            }
        }
    }
    let mut out = lines.join("\n");
    if script.ends_with('\n') {
        out.push('\n');
    }
    // The banner shows the one-liner that fetched this script, and a fork's
    // copy was fetched from the fork. Those two comments are the only other
    // place the script names acs's own releases, so they move with the
    // assignments; for a build that publishes upstream this changes
    // nothing. The releases URL goes first: the project page is a prefix of
    // it.
    let upstream_releases = acs::release::UPSTREAM_RELEASES_URL;
    out = out.replace(upstream_releases, releases);
    out = out.replace(upstream::repo(upstream_releases), upstream::repo(releases));
    // Belt and braces: read the two values back out of what is about to be
    // published and check they are the ones asked for. The whole point of
    // this substitution is that a release's installer agrees with the
    // release's binary, so the release refuses to be packaged if it does
    // not — a wrong installer is found here rather than by a user.
    for (name, value) in [("default_releases", releases), ("release_key", key)] {
        if installer_value(&out, name).as_deref() != Some(value) {
            return Err(format!(
                "the packaged installer's {name} did not come out as {value:?} \
                 but as {:?} (scripts/install.sh, acs-x57)",
                installer_value(&out, name)
            ));
        }
    }
    Ok(out)
}

/// The value of a `name='…'` assignment in a packaged installer, for the
/// tests and for the line `package` prints.
pub fn installer_value(script: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}='");
    script
        .lines()
        .find_map(|l| l.strip_prefix(&prefix)?.strip_suffix('\''))
        .map(|v| v.replace("'\\''", "'"))
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
        // The installer is not copied as it is: it carries this build's
        // releases URL and release key, so a fork's release asset installs
        // the fork's acs and checks the fork's signature (acs-x57).
        let src = std::fs::read_to_string(i).map_err(|e| format!("{}: {e}", i.display()))?;
        let releases = upstream::releases();
        let key = acs::signature::RELEASE_KEY;
        let text =
            install_script(&src, releases, key).map_err(|e| format!("{}: {e}", i.display()))?;
        let dest = out.join("install.sh");
        std::fs::write(&dest, &text).map_err(|e| format!("{}: {e}", dest.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        eprintln!("install.sh: releases {releases}");
        eprintln!("install.sh: release key {key}");
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

    /// acs-x57: the notes send people at whatever this build publishes, so
    /// a fork's release notes do not install acs for its users.
    #[test]
    fn the_notes_point_at_the_builds_own_releases() {
        let a = vec![(
            "aarch64-apple-darwin".to_string(),
            "acs-1.2.3-aarch64-apple-darwin.tar.gz".to_string(),
        )];
        let fork = "https://github.com/someone/acs-fork/releases";
        let n = notes_with("1.2.3", &a, "", fork, Some("someone/acs-fork/acs"));
        assert!(
            n.contains(&format!(
                "curl -fsSL {fork}/latest/download/install.sh | sh"
            )),
            "{n}"
        );
        assert!(
            n.contains(&format!(
                "curl -LO {fork}/download/v1.2.3/acs-1.2.3-aarch64-apple-darwin.tar.gz"
            )),
            "{n}"
        );
        assert!(n.contains("`brew install someone/acs-fork/acs`"), "{n}");
        assert!(!n.contains("michel-onstein"), "{n}");
        // A release that pushes no tap says nothing about Homebrew rather
        // than naming a formula nobody published.
        let n = notes_with("1.2.3", &a, "", fork, None);
        assert!(!n.contains("Homebrew"), "{n}");
        assert!(!n.contains("brew install"), "{n}");
    }

    /// The installer as it is in the repository, which every test here
    /// starts from.
    fn source_installer() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/install.sh"))
            .unwrap()
    }

    const FORK_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPfNoK6xJ0aUTVQO2S4pfnCbFrwVzBc5SkMRqgP4ePxX fork";

    /// acs-x57, and the drift guard that moved here from
    /// `tests/scripts.rs`: the **packaged** installer carries this build's
    /// releases URL and this build's release key, whichever build that is.
    /// An installer that disagrees with the binary is one that installs
    /// someone else's acs, or refuses the release it ships with.
    #[test]
    fn the_packaged_installer_carries_this_builds_url_and_key() {
        let packaged = install_script(
            &source_installer(),
            upstream::releases(),
            acs::signature::RELEASE_KEY,
        )
        .unwrap();
        assert_eq!(
            installer_value(&packaged, "default_releases").as_deref(),
            Some(acs::release::DEFAULT_RELEASES_URL),
            "the packaged installer does not install from this build's releases"
        );
        assert_eq!(
            installer_value(&packaged, "release_key").as_deref(),
            Some(acs::signature::RELEASE_KEY),
            "the packaged installer's release key is not the one built into acs"
        );
        // It is the same script otherwise: only those two lines move.
        let source = source_installer();
        let differ: Vec<(&str, &str)> = source
            .lines()
            .zip(packaged.lines())
            .filter(|(a, b)| a != b)
            .collect();
        for (a, b) in &differ {
            assert!(
                a.starts_with("default_releases=") || a.starts_with("release_key="),
                "{a:?} became {b:?}"
            );
        }
        assert!(differ.len() <= 2, "{differ:?}");

        // A fork's values reach the packaged copy, and nothing of acs's
        // own is left in the two lines that matter.
        let fork = "https://github.com/someone/acs-fork/releases";
        let packaged = install_script(&source, fork, FORK_KEY).unwrap();
        assert_eq!(
            installer_value(&packaged, "default_releases").as_deref(),
            Some(fork)
        );
        assert_eq!(
            installer_value(&packaged, "release_key").as_deref(),
            Some(FORK_KEY)
        );
        assert!(
            !packaged.contains(acs::signature::UPSTREAM_RELEASE_KEY),
            "a fork's installer still carries acs's key"
        );
        // Not even in the banner, which shows the one-liner that fetched
        // this script: for a fork's copy that is the fork's.
        assert!(
            !packaged.contains(acs::release::UPSTREAM_RELEASES_URL),
            "a fork's installer still names acs's releases"
        );
        assert!(
            packaged.contains("# Install acs (https://github.com/someone/acs-fork):"),
            "the banner still sends the reader upstream"
        );
    }

    /// The same guard from the other side: a script that has drifted so
    /// that a value cannot be substituted **fails the packaging** rather
    /// than shipping the value that is already in it.
    #[test]
    fn an_installer_whose_lines_moved_is_refused() {
        let source = source_installer();
        for (name, gone) in [
            ("default_releases", "default_releases="),
            ("release_key", "release_key="),
        ] {
            // Indented, as a line inside an `if` would be: no longer an
            // assignment this can find.
            let moved = source.replace(&format!("\n{gone}"), &format!("\n    {gone}"));
            assert_ne!(moved, source, "{name}");
            let why = install_script(&moved, "https://example.com/r", FORK_KEY).unwrap_err();
            assert!(why.contains(&format!("no `{name}=` line")), "{why}");
        }
        // Written twice — a second default that would win — is refused
        // too, rather than one of the two being left as it was.
        let twice = source.replace(
            "\ndefault_releases=",
            "\ndefault_releases='https://elsewhere.example/r'\ndefault_releases=",
        );
        let why = install_script(&twice, "https://example.com/r", FORK_KEY).unwrap_err();
        assert!(why.contains("on 2 lines"), "{why}");
    }

    /// Substituting into a script that people run with `curl … | sh` is
    /// the security-adjacent part of acs-x57: whatever the build supplies
    /// must become a string and can never become a command. Single quotes
    /// make that total, so the hostile values below survive as text.
    #[test]
    fn a_build_time_value_cannot_inject_shell() {
        let dir = std::env::temp_dir().join(format!("acs-installer-inject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let canary = dir.join("pwned");
        let hostile = [
            format!("https://x/'; touch {}; echo '", canary.display()),
            format!("https://x/$(touch {})", canary.display()),
            format!("https://x/`touch {}`", canary.display()),
            "https://x/\"; rm -rf /; \"".to_string(),
            "https://x/$HOME ${IFS} \\ | & ; ( ) < >".to_string(),
        ];
        for value in &hostile {
            let packaged = install_script(&source_installer(), value, value).unwrap();
            // Run just the two assignments and print what the shell made
            // of them: the script itself would go to the network.
            let lines: String = packaged
                .lines()
                .filter(|l| l.starts_with("default_releases=") || l.starts_with("release_key="))
                .map(|l| format!("{l}\n"))
                .collect();
            assert_eq!(lines.lines().count(), 2, "{lines}");
            let probe = dir.join("probe.sh");
            std::fs::write(
                &probe,
                format!("{lines}printf '%s' \"$default_releases\" > '{0}/a'\nprintf '%s' \"$release_key\" > '{0}/b'\n", dir.display()),
            )
            .unwrap();
            let out = Command::new("sh").arg(&probe).output().unwrap();
            assert!(
                out.status.success(),
                "{value:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            for f in ["a", "b"] {
                assert_eq!(
                    &std::fs::read_to_string(dir.join(f)).unwrap(),
                    value,
                    "{value:?} did not survive as text"
                );
            }
            assert!(!canary.exists(), "{value:?} ran a command");
        }
        // A line break is refused outright: harmless inside the quotes, but
        // it would split the assignment over two lines and leave nothing
        // able to read the value back.
        for bad in [
            "https://x/\ntouch /tmp/x",
            "ssh-ed25519 AAAA\nssh-ed25519 BBBB",
        ] {
            let why = install_script(&source_installer(), bad, bad).unwrap_err();
            assert!(why.contains("line break"), "{bad:?}: {why}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
