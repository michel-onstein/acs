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
//! Either way `_install --finish` checks the SHA-256 the client computed,
//! renames the file into place atomically and repoints `~/.local/bin/acs`.
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
    let dir = format!("\"$HOME\"/.local/share/acs/{}", sh_quote(v));
    let tmp = format!("{dir}/acs.new.{token}");
    match plan {
        Plan::SelfCopy { data } => {
            let digest = sha256::hex(&sha256::digest(&data));
            let script = format!(
                "set -e; mkdir -p {dir}; cat > {tmp}; chmod 755 {tmp}; exec {tmp} _install --finish --token {token} --sha256 {digest}"
            );
            side_call(args, &script, &data)?;
        }
        Plan::Payload { gz, blob, digest } => {
            let slim_digest = sha256::hex(&digest);
            let script = format!(
                "set -e; command -v gzip >/dev/null || {{ echo 'acs: the remote has no gzip' >&2; exit 3; }}; mkdir -p {dir}; gzip -dc > {tmp}; chmod 755 {tmp}"
            );
            side_call(args, &script, &gz)?;
            let script = format!(
                "exec {tmp} _install --finish --token {token} --slim-sha256 {slim_digest} --payloads"
            );
            side_call(args, &script, &blob)?;
        }
    }
    note(&format!("installed acs {v} on {host}"));
    Ok(())
}

/// Run `script` on the host with `input` on stdin; succeed on "ok".
fn side_call(args: &ClientArgs, script: &str, input: &[u8]) -> Result<(), String> {
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
    let mut out = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut out);
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !status.success() {
        return Err(format!("installing acs failed ({status})"));
    }
    // Only the finisher prints ("ok"); the upload step prints nothing.
    if !out.trim().is_empty() && !out.lines().any(|l| l.trim() == "ok") {
        return Err(format!("unexpected reply while installing: {}", out.trim()));
    }
    Ok(())
}

// ---- `acs _install --finish` (runs on the remote) ---------------------------

struct Finish {
    token: String,
    sha256: Option<String>,
    slim_sha256: Option<String>,
    payloads: bool,
}

fn parse_finish(args: &[OsString]) -> Result<Finish, String> {
    let mut f = Finish {
        token: String::new(),
        sha256: None,
        slim_sha256: None,
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
            println!("ok");
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
    link_bin(&final_path, &f.token)?;
    Ok(())
}

fn write_exe(path: &Path, parts: &[&[u8]]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    for p in parts {
        f.write_all(p).map_err(|e| e.to_string())?;
    }
    f.sync_all().map_err(|e| e.to_string())
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("rename to {}: {e}", to.display()))
}

/// Point `~/.local/bin/acs` at the newest install: a temporary symlink
/// renamed over the old one, so it is never missing.
fn link_bin(target: &Path, token: &str) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let bin = PathBuf::from(home).join(".local/bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let tmp = bin.join(format!(".acs.{token}"));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| e.to_string())?;
    rename(&tmp, &bin.join("acs"))
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

    #[test]
    fn finish_arguments() {
        let a = |v: &[&str]| parse_finish(&v.iter().map(OsString::from).collect::<Vec<_>>());
        assert!(a(&["--finish", "--token", "abc123", "--sha256", "00"]).is_ok());
        assert!(a(&["--finish", "--token", "../x"]).is_err());
        assert!(a(&["--token", "ab"]).is_err());
    }
}
