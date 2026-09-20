//! The signature on a release's `SHA256SUMS` (DESIGN §9, acs-o9v).
//!
//! The checksums and the archives come from the same place, so a checksum
//! only proves the archive matches what that server said it should be.
//! Anyone able to write to the release — a leaked token, a taken-over
//! account — replaces both, and `acs upgrade` verifies happily and then
//! *runs* the binary ([`crate::upgrade`]) before installing it onto every
//! remote the user connects to. So the checksums are signed, and the
//! signature is checked against a key built into this binary, before the
//! checksums are believed and long before anything downloaded is run.
//!
//! The signing is `ssh-keygen -Y`, which acs can rely on because acs is an
//! ssh tool: a machine that cannot run `ssh-keygen` cannot run acs. That
//! also lets `scripts/install.sh` — a POSIX shell script that runs before
//! acs exists — make exactly the same check with no extra dependency, and
//! it keeps a signature verifier out of this binary, which carries no
//! cryptography beyond its own SHA-256.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The public half of the key that signs acs releases. The private half
/// signs `SHA256SUMS` in `scripts/release-binaries.sh`; it is not on any
/// machine that only *runs* acs.
///
/// Also published in the Homebrew tap, which is a repository of its own:
/// someone who wants to check this key against a second source, rather
/// than trusting the binary that carries it, has one.
pub const RELEASE_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILbxQW5C9X7CdwcQ4bab0gsQi4Evk2xfgmI/972dlHCb acs release signing";

/// The principal this key signs as, in the allowed-signers file and as
/// `-I` when verifying.
pub const IDENTITY: &str = "releases@acs";

/// The SSHSIG namespace. A signature made for one namespace does not
/// verify in another, so a release signature cannot be replayed as, say,
/// a git commit signature made by the same key.
pub const NAMESPACE: &str = "acs-release";

/// The signature beside a `SHA256SUMS` URL or file.
pub fn signature_of(url: &str) -> String {
    format!("{url}.sig")
}

/// The one line an allowed-signers file needs for [`RELEASE_KEY`].
///
/// `namespaces=` restricts the key to release signatures on this side too,
/// so both ends of the check name the namespace.
fn allowed_signers(key: &str) -> String {
    format!("{IDENTITY} namespaces=\"{NAMESPACE}\" {key}\n")
}

/// The key a release from `base` is checked against.
///
/// For the release channel acs actually ships from this is [`RELEASE_KEY`]
/// and nothing else: there is no environment variable, no flag and no file
/// that can put another key in its place.
///
/// `ACS_RELEASE_KEY` is read only when `base` is *not* that channel — when
/// `ACS_RELEASES_URL` already points acs somewhere else, which the tests do
/// and which `release.rs` only allows over https, refuses outright across a
/// privilege boundary, and otherwise demands `--allow-insecure-url` on the
/// command line for. Whoever can redirect the channel already chooses what
/// acs downloads and runs, so also letting them name that channel's key
/// gives them nothing they did not have. It is what lets a mirror be signed
/// by its own key.
pub fn key_for(base: &str) -> String {
    if base == crate::release::DEFAULT_RELEASES_URL {
        return RELEASE_KEY.to_string();
    }
    std::env::var("ACS_RELEASE_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| RELEASE_KEY.to_string())
}

/// Check `sig` against `sums` and the key for `base`, writing the
/// allowed-signers file `ssh-keygen` needs into `tmp`.
///
/// Every failure is a refusal: a missing `ssh-keygen`, a signature that
/// does not verify and a build with no key in it all return `Err`. There
/// is deliberately no path that skips the check — one that could be
/// skipped would be worth nothing.
pub fn verify(base: &str, sums: &Path, sig: &Path, tmp: &Path) -> Result<(), String> {
    verify_with(&key_for(base), sums, sig, tmp)
}

/// [`verify`] against `key` rather than the built-in one, for the tests.
pub fn verify_with(key: &str, sums: &Path, sig: &Path, tmp: &Path) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err(
            "this acs was built without a release key, so it cannot check the signature".into(),
        );
    }
    if !sig.is_file() {
        return Err(format!(
            "the release has no signature for SHA256SUMS (looked for {})",
            sig.display()
        ));
    }
    let allowed = write_allowed_signers(key, tmp)?;
    let file = std::fs::File::open(sums).map_err(|e| format!("{}: {e}", sums.display()))?;
    let out = Command::new("ssh-keygen")
        .arg("-Y")
        .arg("verify")
        .arg("-f")
        .arg(&allowed)
        .args(["-I", IDENTITY, "-n", NAMESPACE, "-s"])
        .arg(sig)
        .stdin(Stdio::from(file))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "acs needs ssh-keygen to check the release signature".to_string()
            }
            _ => format!("cannot run ssh-keygen: {e}"),
        });
    let _ = std::fs::remove_file(&allowed);
    let out = out?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr);
    let why = why.lines().next().unwrap_or("it does not verify").trim();
    Err(format!(
        "the release's SHA256SUMS is not signed by the acs release key ({why}); nothing was downloaded"
    ))
}

/// Write the allowed-signers file, readable only by us: `ssh-keygen`
/// takes the key from a file, and a file another user can write is a file
/// that can name another key.
fn write_allowed_signers(key: &str, tmp: &Path) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = tmp.join(format!("allowed_signers.{}", crate::sys::random_token()));
    // `create_new` is O_CREAT|O_EXCL, so a name planted ahead of us is an
    // error rather than a write through to its target (acs-721).
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(allowed_signers(key).as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A throwaway signing key; returns its public half.
    fn keygen(dir: &Path, name: &str) -> String {
        let key = dir.join(name);
        let st = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&key)
            .status()
            .expect("ssh-keygen");
        assert!(st.success());
        std::fs::read_to_string(key.with_extension("pub"))
            .unwrap()
            .trim()
            .to_string()
    }

    /// Sign `sums` with `name`'s private half, into `sums`.sig.
    fn sign(dir: &Path, name: &str, sums: &Path, namespace: &str) -> PathBuf {
        let st = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", namespace, "-f"])
            .arg(dir.join(name))
            .arg(sums)
            .status()
            .expect("ssh-keygen -Y sign");
        assert!(st.success());
        PathBuf::from(format!("{}.sig", sums.display()))
    }

    fn sums_file(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("SHA256SUMS");
        std::fs::write(&p, text).unwrap();
        p
    }

    const SUMS: &str = "abc  acs-1.2.3-x86_64-unknown-linux-musl.tar.gz\n";

    #[test]
    fn a_signature_from_the_release_key_verifies() {
        let dir = TempDir::new();
        let key = keygen(dir.path(), "k");
        let sums = sums_file(dir.path(), SUMS);
        let sig = sign(dir.path(), "k", &sums, NAMESPACE);
        assert_eq!(verify_with(&key, &sums, &sig, dir.path()), Ok(()));
        // Nothing is left behind: the allowed-signers file is removed.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("allowed_signers"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn checksums_changed_after_signing_do_not_verify() {
        let dir = TempDir::new();
        let key = keygen(dir.path(), "k");
        let sums = sums_file(dir.path(), SUMS);
        let sig = sign(dir.path(), "k", &sums, NAMESPACE);
        // The attack the signature is for: the checksums are swapped for
        // ones matching a planted archive, the signature left as it was.
        std::fs::write(&sums, "def  acs-1.2.3-x86_64-unknown-linux-musl.tar.gz\n").unwrap();
        let e = verify_with(&key, &sums, &sig, dir.path()).unwrap_err();
        assert!(e.contains("is not signed by the acs release key"), "{e}");
        assert!(e.contains("nothing was downloaded"), "{e}");
    }

    #[test]
    fn another_key_does_not_verify() {
        let dir = TempDir::new();
        let ours = keygen(dir.path(), "ours");
        let sums = sums_file(dir.path(), SUMS);
        // Signed by a key that is not ours, as a release published by
        // someone who took over the account would be.
        keygen(dir.path(), "theirs");
        let sig = sign(dir.path(), "theirs", &sums, NAMESPACE);
        let e = verify_with(&ours, &sums, &sig, dir.path()).unwrap_err();
        assert!(e.contains("is not signed by the acs release key"), "{e}");
    }

    #[test]
    fn a_signature_for_another_namespace_does_not_verify() {
        let dir = TempDir::new();
        let key = keygen(dir.path(), "k");
        let sums = sums_file(dir.path(), SUMS);
        // The same key signing something else — a git commit, say — must
        // not produce a signature that passes as a release signature.
        let sig = sign(dir.path(), "k", &sums, "git");
        let e = verify_with(&key, &sums, &sig, dir.path()).unwrap_err();
        assert!(e.contains("is not signed by the acs release key"), "{e}");
    }

    #[test]
    fn a_missing_signature_is_a_refusal_not_a_pass() {
        let dir = TempDir::new();
        let key = keygen(dir.path(), "k");
        let sums = sums_file(dir.path(), SUMS);
        let e = verify_with(&key, &sums, &dir.path().join("nope.sig"), dir.path()).unwrap_err();
        assert!(e.contains("no signature for SHA256SUMS"), "{e}");
    }

    #[test]
    fn a_build_without_a_key_refuses_rather_than_skips() {
        let dir = TempDir::new();
        let sums = sums_file(dir.path(), SUMS);
        let key = keygen(dir.path(), "k");
        let sig = sign(dir.path(), "k", &sums, NAMESPACE);
        for empty in ["", "   ", "\n"] {
            let e = verify_with(empty, &sums, &sig, dir.path()).unwrap_err();
            assert!(e.contains("built without a release key"), "{empty:?}: {e}");
        }
        // The real one is not empty: this build can check a signature.
        assert!(
            !RELEASE_KEY.trim().is_empty(),
            "no release key is built in; releases cannot be checked"
        );
        let _ = key;
    }

    /// The release channel acs ships from takes the built-in key and no
    /// other, whatever the environment says. Only a redirected channel —
    /// which already chooses what acs runs — may name its own key.
    #[test]
    fn the_default_channel_takes_no_key_from_the_environment() {
        // Read the decision directly rather than setting the variable:
        // tests share one environment and run in parallel.
        assert_eq!(
            key_for(crate::release::DEFAULT_RELEASES_URL),
            RELEASE_KEY,
            "the default channel must use the built-in key"
        );
        // A redirected one falls back to the built-in key when nothing
        // names another, so a mirror is not unchecked by default.
        let mirror = "https://mirror.example.com/releases";
        assert!(std::env::var_os("ACS_RELEASE_KEY").is_none());
        assert_eq!(key_for(mirror), RELEASE_KEY);
    }

    #[test]
    fn the_signature_sits_beside_the_checksums() {
        assert_eq!(
            signature_of("https://example.com/r/latest/download/SHA256SUMS"),
            "https://example.com/r/latest/download/SHA256SUMS.sig"
        );
    }

    #[test]
    fn the_allowed_signers_line_names_the_identity_and_namespace() {
        let line = allowed_signers("ssh-ed25519 AAAAC3 test");
        assert_eq!(
            line,
            "releases@acs namespaces=\"acs-release\" ssh-ed25519 AAAAC3 test\n"
        );
    }
}
