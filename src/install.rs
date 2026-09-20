//! Installing acs on a remote on first contact (DESIGN §8, §8.1).
//!
//! The client learns what is missing from the prelude's `ACS-NEED <os>
//! <arch>` line and installs exactly its own version under
//! `~/.local/share/acs/<version>/acs`:
//!
//! - **self-copy** — the remote has this binary's own target: stream our file
//!   (complete if it carries payloads) in one call;
//! - **payload** — another Linux target we carry: stream its gzip'd slim
//!   build to the remote's `gzip -dc`, then run the new binary's
//!   `_install --finish` with the payload set on stdin, which appends it.
//!
//! Either way the **remote shell** checks the SHA-256 the client computed
//! before the upload is made executable (acs-4km), and `_install --finish`
//! then renames the file into place atomically and repoints
//! `~/.local/bin/acs`.
//! A running executable cannot be written to on Linux (ETXTBSY), so the
//! finisher writes a new file instead of appending to itself.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cli::ClientArgs;
use crate::client::note;
use crate::payload::{self, Payloads};
use crate::sha256;
use crate::ssh::{sh_quote, Call};

static ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// What to send for `target`, decided from what this binary carries.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Our own file, streamed as is.
    SelfCopy { data: Vec<u8> },
    /// A gzip'd slim build, then the payload blob appended remotely.
    Payload {
        gz: Vec<u8>,
        blob: Vec<u8>,
        digest: [u8; 32],
    },
}

/// Decide how to install acs for `target` given our own target, our file,
/// and the payloads we carry.
pub fn plan(
    target: &str,
    own_target: &str,
    own_file: &[u8],
    payloads: Option<&Payloads>,
) -> Result<Plan, String> {
    if target == own_target {
        return Ok(Plan::SelfCopy {
            data: own_file.to_vec(),
        });
    }
    if payload::is_elf_target(target) {
        if let Some(p) = payloads {
            if let Some((entry, gz)) = p.get(target) {
                // `digest` is the slim build's: the finisher checks what
                // gzip unpacked before completing it with the blob.
                return Ok(Plan::Payload {
                    gz: gz.to_vec(),
                    blob: p.blob().to_vec(),
                    digest: entry.sha256,
                });
            }
        }
        return Err(format!(
            "this acs build carries no binary for {target}; build a complete one with `cargo xtask dist`, or install ~/.local/share/acs/{}/acs on the host by hand",
            crate::VERSION
        ));
    }
    Err(format!(
        "cannot install acs for {target} from a {own_target} build; install ~/.local/share/acs/{}/acs on the host by hand",
        crate::VERSION
    ))
}

/// The one-line installer: it puts a release where the prelude looks for it.
pub const INSTALLER: &str =
    "https://github.com/michel-onstein/acs/releases/latest/download/install.sh";

/// What to say when the remote lacks acs and `install_on_remote` is off.
pub fn not_installing(args: &ClientArgs, os: &str, arch: &str) -> String {
    let v = crate::VERSION;
    let why = match &args.config.install_on_remote.origin {
        Some(o) => format!(" ({o})"),
        None => String::new(),
    };
    format!(
        "acs {v} is not installed on {host} ({os} {arch}), and install_on_remote is false{why}; \
         install it there with `curl -fsSL {INSTALLER} | ACS_VERSION={v} sh`, or set install_on_remote: true",
        host = args.transport.destination
    )
}

/// The remote has no acs of our version for `os`/`arch`: install it.
pub fn install(args: &ClientArgs, os: &str, arch: &str) -> Result<(), String> {
    let host = &args.transport.destination;
    if ATTEMPTED.swap(true, Ordering::SeqCst) {
        return Err(format!(
            "acs was installed on {host} but the remote still cannot run it (is $HOME the same for ssh commands?)"
        ));
    }
    let target = payload::target_for_uname(os, arch)
        .ok_or_else(|| format!("{host} runs {os} {arch}, which acs does not support"))?;
    let own = crate::sys::self_exe()
        .and_then(std::fs::read)
        .map_err(|e| format!("cannot read this acs binary: {e}"))?;
    let carried = Payloads::from_self();
    let plan = plan(target, payload::OWN_TARGET, &own, carried.as_ref())?;
    note(&format!(
        "installing acs {} on {host} ({os} {arch})…",
        crate::VERSION
    ));
    let token = format!("{:016x}", crate::sys::random_u64());
    let v = crate::VERSION;
    let acs_dir = "\"$HOME\"/.local/share/acs";
    let dir = format!("{acs_dir}/{}", sh_quote(v));
    let tmp = format!("{dir}/acs.new.{token}");
    let make_dir = make_dir(acs_dir, &dir);
    match plan {
        Plan::SelfCopy { data } => {
            let digest = sha256::hex(&sha256::digest(&data));
            let check = check_upload(&tmp, &digest);
            let script = format!(
                "set -e; {make_dir} cat > {tmp}; {check} chmod 755 {tmp}; exec {tmp} _install --finish --token {token} --sha256 {digest}"
            );
            side_call(args, &script, &data, true)?;
        }
        Plan::Payload { gz, blob, digest } => {
            let slim_digest = sha256::hex(&digest);
            let blob_digest = sha256::hex(&sha256::digest(&blob));
            let check = check_upload(&tmp, &slim_digest);
            let script = format!(
                "set -e; command -v gzip >/dev/null || {{ echo 'acs: the remote has no gzip' >&2; exit 3; }}; {make_dir} gzip -dc > {tmp}; {check} chmod 755 {tmp}"
            );
            side_call(args, &script, &gz, false)?;
            let script = format!(
                "exec {tmp} _install --finish --token {token} --slim-sha256 {slim_digest} --blob-sha256 {blob_digest} --payloads"
            );
            side_call(args, &script, &blob, true)?;
        }
    }
    note(&format!("installed acs {v} on {host}"));
    Ok(())
}

/// The fragment that makes the directory acs installs into, with a mode of
/// its own rather than whatever the remote's umask leaves (acs-iws).
///
/// The prelude refuses to exec a binary whose directory is writable by group
/// or other (acs-08m, `ssh.rs`). A bare `mkdir -p` under the `umask 002`
/// that lab and appliance images still ship makes that directory 0775, so
/// acs installed into a directory it then refused to run from: the install
/// said it had succeeded and the very next connection exited 254.
///
/// `umask 022` covers the parents `mkdir -p` creates on a fresh host
/// (`~/.local`, `~/.local/share`); the `chmod` sets the two directories acs
/// owns, which also heals an install an earlier version left group-writable.
/// Neither touches `$HOME`, `~/.local` or `~/.local/share` when they already
/// exist — acs did not make them, and the prelude does not look at them.
/// The binary's own mode is set past the umask already (acs-28b).
fn make_dir(acs_dir: &str, dir: &str) -> String {
    format!("umask 022; mkdir -p {dir}; chmod 755 {acs_dir} {dir};")
}

/// Shell that checks the uploaded `file` against `want` **before** the file
/// is made executable or run (acs-4km).
///
/// The digest used to be checked by the uploaded binary itself, which is no
/// check at all: a substituted binary skips it and prints `ok`, which is all
/// the client looks for. Anyone who could replace the file between the `cat`
/// and the `exec` — a second person on a shared account, a remote whose
/// `$HOME` others can write — had their code run, and then installed where
/// every later connection execs it.
///
/// So the *remote shell* checks it, with whichever of the three usual tools
/// the host has. A host with none of them cannot be installed onto this way,
/// and says so rather than running something unchecked. The hash is pulled
/// out by shape (64 hex characters) so the differing output formats of
/// `sha256sum`, `shasum` and `openssl dgst` all work.
pub fn check_upload(file: &str, want: &str) -> String {
    format!(
        "got=$({{ sha256sum {file} || shasum -a 256 {file} || openssl dgst -sha256 {file}; }} \
         2>/dev/null | sed -n 's/.*\\([0-9a-f]\\{{64\\}}\\).*/\\1/p' | head -n 1) || true; \
         [ -n \"$got\" ] || {{ rm -f {file}; \
         echo 'acs: the remote has no sha256sum, shasum or openssl to check the upload' >&2; \
         exit 4; }}; \
         [ \"$got\" = {want} ] || {{ rm -f {file}; \
         echo 'acs: the uploaded acs does not match the checksum the client computed' >&2; \
         exit 5; }}; "
    )
}

/// How much of the remote's answer to an install call is read. The reply
/// is one `ok` line under a login banner; anything past this is a host
/// filling memory rather than answering (acs-rip).
const MAX_REPLY: usize = 256 * 1024;

/// Run `script` on the host with `input` on stdin. With `expect_ok` the
/// finisher must print its `ok` line; without, the exit status alone
/// decides — stdout may hold login-shell noise either way (DESIGN §3).
fn side_call(args: &ClientArgs, script: &str, input: &[u8], expect_ok: bool) -> Result<(), String> {
    let remote = crate::ssh::remote_command(script);
    let mut cmd = args.transport.command(Call::Side, &remote);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run ssh: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let r = stdin.write_all(&data);
        drop(stdin);
        r
    });
    // Bytes, not a String: a login banner in Latin-1 or CP437 is not
    // UTF-8, and read_to_string would throw the whole reply away (acs-vdl).
    // Bounded: the finisher answers with one short line, so a host that
    // streams instead is not answering, and reading it to the end would
    // let it grow the client until the machine gives out (acs-rip).
    let mut buf = Vec::new();
    let mut stdout = child.stdout.take().unwrap();
    let _ = std::io::copy(
        &mut (&mut stdout).take(MAX_REPLY as u64),
        &mut io::Cursor::new(&mut buf),
    );
    let out = String::from_utf8_lossy(&buf);
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !status.success() {
        return Err(format!("installing acs failed ({status})"));
    }
    if expect_ok && !out.lines().any(|l| l.trim() == "ok") {
        return Err(format!(
            "unexpected reply while installing: {:?}",
            out.trim()
        ));
    }
    Ok(())
}

// ---- `acs _install --finish` (runs on the remote) ---------------------------

struct Finish {
    token: String,
    sha256: Option<String>,
    slim_sha256: Option<String>,
    /// Digest of the payload trailer that arrives on stdin (acs-4km). The
    /// blob used to be accepted on the strength of parsing, which only says
    /// it is well formed, not that it is ours.
    blob_sha256: Option<String>,
    payloads: bool,
}

fn parse_finish(args: &[OsString]) -> Result<Finish, String> {
    let mut f = Finish {
        token: String::new(),
        sha256: None,
        slim_sha256: None,
        blob_sha256: None,
        payloads: false,
    };
    let mut finish = false;
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--finish" => finish = true,
            "--token" => f.token = it.next().ok_or("--token needs a value")?,
            "--sha256" => f.sha256 = Some(it.next().ok_or("--sha256 needs a value")?),
            "--slim-sha256" => {
                f.slim_sha256 = Some(it.next().ok_or("--slim-sha256 needs a value")?)
            }
            "--blob-sha256" => {
                f.blob_sha256 = Some(it.next().ok_or("--blob-sha256 needs a value")?)
            }
            "--payloads" => f.payloads = true,
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    if !finish {
        return Err("usage: acs _install --finish ...".into());
    }
    if f.token.is_empty() || !f.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("bad --token".into());
    }
    Ok(f)
}

/// Entry point of `acs _install` on the remote.
pub fn finish_main(args: &[OsString]) -> ExitCode {
    match parse_finish(args).and_then(|f| finish(&f)) {
        Ok(()) => {
            // On a line of its own even after unterminated login noise.
            print!("\nok\n");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("acs: install: {e}");
            ExitCode::from(1)
        }
    }
}

fn finish(f: &Finish) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = me.parent().ok_or("no install directory")?.to_path_buf();
    let expected_tmp = dir.join(format!("acs.new.{}", f.token));
    if me != expected_tmp
        && std::fs::canonicalize(&me).ok() != std::fs::canonicalize(&expected_tmp).ok()
    {
        return Err(format!(
            "run from {}, expected {}",
            me.display(),
            expected_tmp.display()
        ));
    }
    let final_path = dir.join("acs");
    if f.payloads {
        // Our own file is the slim build; complete it with the blob.
        let slim = std::fs::read(&me).map_err(|e| e.to_string())?;
        if let Some(want) = &f.slim_sha256 {
            if &sha256::hex(&sha256::digest(&slim)) != want {
                let _ = std::fs::remove_file(&me);
                return Err("the uploaded binary is corrupt (checksum mismatch)".into());
            }
        }
        let mut blob = Vec::new();
        io::stdin()
            .read_to_end(&mut blob)
            .map_err(|e| e.to_string())?;
        if let Some(want) = &f.blob_sha256 {
            if &sha256::hex(&sha256::digest(&blob)) != want {
                let _ = std::fs::remove_file(&me);
                return Err("the payload set does not match its checksum".into());
            }
        }
        if Payloads::parse(&blob).is_none() {
            let _ = std::fs::remove_file(&me);
            return Err("the payload set is corrupt".into());
        }
        let full = dir.join(format!("acs.full.{}", f.token));
        write_exe(&full, &[&slim, &blob])?;
        rename(&full, &final_path)?;
        let _ = std::fs::remove_file(&me);
    } else {
        let data = std::fs::read(&me).map_err(|e| e.to_string())?;
        if let Some(want) = &f.sha256 {
            if &sha256::hex(&sha256::digest(&data)) != want {
                let _ = std::fs::remove_file(&me);
                return Err("the uploaded binary is corrupt (checksum mismatch)".into());
            }
        }
        rename(&me, &final_path)?;
    }
    if let Err(e) = link_bin(&final_path, &f.token) {
        // The link is a convenience: nothing in the protocol depends on it
        // (DESIGN §8), and the client finds the binary through the prelude.
        // So it is a warning after the ok, not a failed install (acs-iry).
        eprintln!(
            "acs: warning: cannot link ~/.local/bin/acs: {e} (acs is installed at {})",
            final_path.display()
        );
    }
    Ok(())
}

fn write_exe(path: &Path, parts: &[&[u8]]) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    for p in parts {
        f.write_all(p).map_err(|e| e.to_string())?;
    }
    f.sync_all().map_err(|e| e.to_string())?;
    // The mode above is masked by the remote's umask (077 leaves 0700), so
    // set it for real, as the self-copy path does (acs-28b).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("rename to {}: {e}", to.display()))
}

/// Point `~/.local/bin/acs` at the newest install: a temporary symlink
/// renamed over the old one, so it is never missing.
fn link_bin(target: &Path, token: &str) -> Result<(), String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let bin = home.join(".local/bin");
    let link = bin.join("acs");
    if !should_link(&link, target, &home.join(".local/share/acs")) {
        return Ok(());
    }
    std::fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let tmp = bin.join(format!(".acs.{token}"));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| e.to_string())?;
    rename(&tmp, &link)
}

/// Whether `~/.local/bin/acs` (`link`) should point at `target`, the
/// version just installed under `share`: when it is missing, or a link of
/// ours (into `share`) to an older or vanished version. A file or link the
/// user put there is left alone, and a newer version keeps the link.
fn should_link(link: &Path, target: &Path, share: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(link) {
        // Nothing there: link. Anything else wrong with the path (~/.local
        // /bin is a regular file): try, so the failure is said out loud
        // rather than passed over (acs-iry).
        Err(_) => return true,
        Ok(m) => m,
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    let Ok(current) = std::fs::read_link(link) else {
        return false;
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let share_c = canon(share);
    // A dangling link cannot be canonicalised: judge it by its parent.
    let current_c = match std::fs::canonicalize(&current) {
        Ok(c) => c,
        Err(_) => current
            .parent()
            .map(|d| canon(d).join(current.file_name().unwrap_or_default()))
            .unwrap_or_else(|| current.clone()),
    };
    if !(current.starts_with(share) || current_c.starts_with(&share_c)) {
        return false;
    }
    if !current_c.exists() {
        return true;
    }
    let version = |p: &Path| {
        p.parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .map(str::to_string)
    };
    match (version(&current_c), version(target)) {
        (Some(have), Some(new)) => crate::release::compare(&new, &have).map_or(true, |o| o.is_ge()),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_for(target: &str, raw: &[u8]) -> Payloads {
        Payloads::parse(&payload::build(&[payload::Input {
            target,
            raw,
            gz: b"GZ",
        }]))
        .unwrap()
    }

    #[test]
    fn same_target_is_a_self_copy() {
        let p = plan("aarch64-apple-darwin", "aarch64-apple-darwin", b"me", None).unwrap();
        assert_eq!(
            p,
            Plan::SelfCopy {
                data: b"me".to_vec()
            }
        );
    }

    #[test]
    fn other_linux_target_uses_the_payload() {
        let carried = blob_for("aarch64-unknown-linux-musl", b"slim-arm");
        match plan(
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
            b"me",
            Some(&carried),
        )
        .unwrap()
        {
            Plan::Payload { gz, blob, digest } => {
                assert_eq!(gz, b"GZ");
                assert_eq!(blob, carried.blob());
                assert_eq!(digest, sha256::digest(b"slim-arm"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_payload_explains_how_to_get_one() {
        let e = plan(
            "x86_64-unknown-linux-musl",
            "aarch64-apple-darwin",
            b"me",
            None,
        )
        .unwrap_err();
        assert!(e.contains("cargo xtask dist"), "{e}");
        let e = plan("x86_64-apple-darwin", "aarch64-apple-darwin", b"me", None).unwrap_err();
        assert!(e.contains("by hand"), "{e}");
    }

    /// Regression (acs-tb1): `~/.local/bin/acs` is only ever pointed at a
    /// newer version of ours, never over the user's own file or link.
    #[test]
    fn the_bin_link_is_only_moved_forward() {
        let d = crate::testutil::TempDir::new();
        let share = d.path().join("share/acs");
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let version = |v: &str| {
            let p = share.join(v).join("acs");
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, v).unwrap();
            p
        };
        let new = version("0.2.0");
        let link = bin.join("acs");
        let set = |to: &Path| {
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(to, &link).unwrap();
        };

        assert!(should_link(&link, &new, &share), "missing");
        set(&version("0.1.9"));
        assert!(should_link(&link, &new, &share), "older");
        set(&new);
        assert!(should_link(&link, &new, &share), "same");
        set(&version("0.10.0"));
        assert!(!should_link(&link, &new, &share), "newer stays");
        set(&share.join("0.0.1/acs"));
        assert!(should_link(&link, &new, &share), "dangling, pruned");
        set(Path::new("/bin/sh"));
        assert!(!should_link(&link, &new, &share), "someone else's link");
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "hand-installed").unwrap();
        assert!(!should_link(&link, &new, &share), "a regular file");
    }

    #[test]
    fn finish_arguments() {
        let a = |v: &[&str]| parse_finish(&v.iter().map(OsString::from).collect::<Vec<_>>());
        assert!(a(&["--finish", "--token", "abc123", "--sha256", "00"]).is_ok());
        assert!(a(&["--finish", "--token", "../x"]).is_err());
        assert!(a(&["--token", "ab"]).is_err());
    }
}
