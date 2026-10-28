//! Installing acs on a remote on first contact (DESIGN §8, §8.1).
//!
//! The client learns what is missing from the prelude's `ACS-NEED <os>
//! <arch>` line and installs exactly its own version under
//! `~/.local/share/acs/<version>/acs`, **on the same connection** (acs-6k2):
//! the prelude waits for a sync line and a one-line script on stdin
//! ([`crate::ssh::prelude_installing`]), and the script reads the bytes it
//! was told to from the same stdin:
//!
//! - **self-copy** — the remote has this binary's own target: our file
//!   (complete if it carries payloads);
//! - **payload** — another Linux target we carry: its gzip'd slim build,
//!   unpacked by the remote's `gzip -dc`, then the payload set, which the
//!   new binary's `_install --finish` appends.
//!
//! Either way the **remote shell** checks the SHA-256 the client computed
//! before the upload is made executable (acs-4km), and `_install --finish`
//! then renames the file into place atomically and repoints
//! `~/.local/bin/acs`, with `~/.local/bin/acscp -> acs` beside it. The prelude then runs the candidates again, with
//! every check it makes on any connection, and execs the proxy on what is
//! left of stdin.
//! A running executable cannot be written to on Linux (ETXTBSY), so the
//! finisher writes a new file instead of appending to itself.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use crate::cli::Connection;
use crate::client::note;
use crate::payload::{self, Payloads};
use crate::sha256;
use crate::ssh::sh_quote;

/// The destinations an install has been attempted on, each at most once
/// per process: a second `ACS-NEED` after one means the install did not
/// take. Per destination, since a copy between two hosts may install on
/// both (ACSCP_DESIGN §5.6, §10.3 item 7).
static ATTEMPTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What to send for `target`, decided from what this binary carries.
///
/// It borrows from the payloads rather than copying them, and the self-copy
/// carries nothing: our own file is read only once that is the plan, and
/// the payloads only when it is not (acs-zgof).
#[derive(Debug, PartialEq, Eq)]
pub enum Plan<'a> {
    /// Our own file, streamed as is.
    SelfCopy,
    /// A gzip'd slim build, then the payload blob appended remotely.
    Payload {
        gz: &'a [u8],
        blob: &'a [u8],
        digest: [u8; 32],
    },
}

/// Decide how to install acs for `target` given our own target and the
/// payloads we carry.
pub fn plan<'a>(
    target: &str,
    own_target: &str,
    payloads: Option<&'a Payloads>,
) -> Result<Plan<'a>, String> {
    if target == own_target {
        return Ok(Plan::SelfCopy);
    }
    if payload::is_elf_target(target) {
        if let Some(p) = payloads {
            if let Some((entry, gz)) = p.get(target) {
                // `digest` is the slim build's: the finisher checks what
                // gzip unpacked before completing it with the blob.
                return Ok(Plan::Payload {
                    gz,
                    blob: p.blob(),
                    digest: entry.sha256,
                });
            }
        }
        return Err(format!(
            concat!(
                "this acs build carries no binary for {}; build a complete one with `cargo xtask dist`, or install ~/",
                crate::layout::user_root_str!(),
                "/{}/acs on the host by hand"
            ),
            target,
            crate::VERSION
        ));
    }
    Err(format!(
        concat!(
            "cannot install acs for {} from a {} build; install ~/",
            crate::layout::user_root_str!(),
            "/{}/acs on the host by hand"
        ),
        target,
        own_target,
        crate::VERSION
    ))
}

/// The one-line installer: it puts a release where the prelude looks for
/// it. It travels with each release, so it is the one beside this build's
/// releases ([`crate::release::DEFAULT_RELEASES_URL`]) — a fork's users
/// are told to install the fork, not upstream (acs-5em).
pub fn installer() -> String {
    format!(
        "{}/latest/download/install.sh",
        crate::release::DEFAULT_RELEASES_URL
    )
}

/// What to say when the remote lacks acs and `install_on_remote` is off.
pub fn not_installing(conn: &Connection, os: &str, arch: &str) -> String {
    let v = crate::VERSION;
    let installer = installer();
    let why = match &conn.config.install_on_remote.origin {
        Some(o) => format!(" ({o})"),
        None => String::new(),
    };
    format!(
        "acs {v} is not installed on {host} ({os} {arch}), and install_on_remote is false{why}; \
         install it there with `curl -fsSL {installer} | ACS_VERSION={v} sh`, or set install_on_remote: true",
        host = conn.transport.destination
    )
}

/// The remote has no acs of our version for `os`/`arch`: install it over
/// `to`, the stdin of the connection whose prelude
/// ([`crate::ssh::prelude_installing`] with `sync`) has just said
/// `ACS-NEED`, then send `hello` for the proxy the prelude execs once the
/// install is in place (acs-6k2).
///
/// `Err` is a plan that cannot be carried out, before anything is sent.
/// A write that fails is not reported here: the remote stopped reading,
/// and its exit status ([`failed`]) says why far better than `EPIPE`.
pub fn install(
    conn: &Connection,
    os: &str,
    arch: &str,
    sync: &str,
    to: RawFd,
    hello: &[u8],
) -> Result<(), String> {
    let host = &conn.transport.destination;
    let mut tried = ATTEMPTED.lock().unwrap_or_else(|e| e.into_inner());
    if tried.contains(host) {
        return Err(format!(
            "acs was installed on {host} but the remote still cannot run it (is $HOME the same for ssh commands?)"
        ));
    }
    tried.push(host.clone());
    drop(tried);
    let target = payload::target_for_uname(os, arch)
        .ok_or_else(|| format!("{host} runs {os} {arch}, which acs does not support"))?;
    // A self-copy sends our file and needs no payloads; anything else
    // needs only the payloads, which are borrowed, not copied (acs-zgof).
    let carried = if target == payload::OWN_TARGET {
        None
    } else {
        Payloads::from_self()
    };
    let plan = plan(target, payload::OWN_TARGET, carried.as_ref())?;
    let own;
    let token = crate::sys::random_token();
    let (script, parts): (String, [&[u8]; 2]) = match plan {
        Plan::SelfCopy => {
            own = crate::sys::self_exe()
                .and_then(std::fs::read)
                .map_err(|e| format!("cannot read this acs binary: {e}"))?;
            let digest = sha256::hex(&sha256::digest(&own));
            (self_copy_script(&token, own.len(), &digest), [&own, &[]])
        }
        Plan::Payload { gz, blob, digest } => {
            let script = payload_script(
                &token,
                (gz.len(), &sha256::hex(&digest)),
                (blob.len(), &sha256::hex(&sha256::digest(blob))),
            );
            (script, [gz, blob])
        }
    };
    note(&format!(
        "installing acs {} on {host} ({os} {arch})…",
        crate::VERSION
    ));
    // In parts, where they lie, rather than one copy of a binary-sized
    // buffer (acs-zgof). Nothing comes back on stdout while this blocks:
    // the login noise came before ACS-NEED, and the script sends every
    // step's output elsewhere.
    let _ = [&header(sync, &script)[..], parts[0], parts[1], hello]
        .iter()
        .try_for_each(|p| crate::sys::write_all(to, p));
    Ok(())
}

/// What the client says when the connection it installed over ended
/// before the new acs answered, given the remote's exit status.
pub fn failed(host: &str, status: Option<i32>) -> String {
    match status {
        Some(crate::ssh::INSTALLED_BUT_REFUSED) => format!(
            "acs {} was installed on {host}, but the remote refused to run it (see above)",
            crate::VERSION
        ),
        Some(n) => format!("installing acs on {host} failed (exit status {n})"),
        None => format!("installing acs on {host} failed (the connection was killed)"),
    }
}

/// The two lines that start an install on a connection waiting in
/// [`crate::ssh::prelude_installing`]: the sync line, after a newline of its
/// own so that a HELLO cut short cannot run into it, and the script.
pub fn header(sync: &str, script: &str) -> Vec<u8> {
    debug_assert!(!script.contains('\n'), "the script must be one line");
    format!("\n{} {sync}\n{script}\n", crate::proto::INSTALL_MARKER).into_bytes()
}

/// Where the install's files go: the version directory, and in it the
/// temporary names for `token`. Random per install, so two clients
/// installing one version at once never write the same file.
struct Paths {
    acs_dir: &'static str,
    dir: String,
    tmp: String,
}

fn paths(token: &str) -> Paths {
    let acs_dir = concat!("\"$HOME\"/", crate::layout::user_root_str!());
    let dir = format!("{acs_dir}/{}", sh_quote(crate::VERSION));
    let tmp = format!("{dir}/acs.new.{token}");
    Paths { acs_dir, dir, tmp }
}

/// The install script for a self-copy of `len` bytes whose SHA-256 is
/// `digest`: take them, check them, and only then make them executable
/// and run the finisher.
pub fn self_copy_script(token: &str, len: usize, digest: &str) -> String {
    let Paths { acs_dir, dir, tmp } = paths(token);
    format!(
        "{TAKE} acs_rm() {{ rm -f {tmp}; }}; trap acs_rm EXIT; set -e; {make_dir} \
         acs_take {len} {tmp}; {check} chmod 755 {tmp}; \
         {tmp} _install --finish --token {token} --sha256 {digest} </dev/null >/dev/null",
        make_dir = make_dir(acs_dir, &dir),
        check = check_upload(&tmp, digest),
    )
}

/// The install script for a payload: `gz` is the gzip'd slim build's length
/// and the slim build's digest, `blob` the payload set's. The slim build is
/// checked before it is made executable, and the payload set before the
/// finisher reads it.
pub fn payload_script(token: &str, gz: (usize, &str), blob: (usize, &str)) -> String {
    let Paths { acs_dir, dir, tmp } = paths(token);
    let gz_file = format!("{dir}/acs.gz.{token}");
    let blob_file = format!("{dir}/acs.blob.{token}");
    format!(
        "{TAKE} acs_rm() {{ rm -f {tmp} {gz_file} {blob_file}; }}; trap acs_rm EXIT; set -e; \
         command -v gzip >/dev/null || {{ echo 'acs: the remote has no gzip' >&2; exit 3; }}; \
         {make_dir} acs_take {gz_len} {gz_file}; gzip -dc < {gz_file} > {tmp}; \
         {check_slim} chmod 755 {tmp}; \
         acs_take {blob_len} {blob_file}; {check_blob} \
         {tmp} _install --finish --token {token} --slim-sha256 {slim} --blob-sha256 {blob_digest} \
         --payloads < {blob_file} >/dev/null",
        make_dir = make_dir(acs_dir, &dir),
        gz_len = gz.0,
        blob_len = blob.0,
        slim = gz.1,
        blob_digest = blob.1,
        check_slim = check_upload(&tmp, gz.1),
        check_blob = check_upload(&blob_file, blob.1),
    )
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

/// `acs_take <n> <file>`: exactly `n` bytes of stdin into `file`, and not
/// one more — what follows is the next step's, and in the end the
/// session's (acs-6k2).
///
/// `head -c` would be shorter, but it is not POSIX and the BSD one reads
/// through stdio, which takes a buffer's worth past the count. A single
/// `read(2)` never returns more than it is asked for, and `dd bs=<rest>
/// count=1` is one read — it does not fill the block on a short one
/// unless told to — so each pass takes what the pipe holds, up to what is
/// still owed, and `wc -c` says how far that got. A pass that gets
/// nothing is the end of input.
///
/// A pipe holds a few dozen kilobytes, so that is a `dd` and a `wc` per
/// few dozen kilobytes — slower than the handshakes it replaces, over a
/// fast link, on a host where a fork is dear. Where `dd` knows
/// `iflag=fullblock` (GNU, busybox) it is told to fill the block, which
/// it does with reads of what is still owed, so the first pass takes the
/// lot; where it does not (the BSDs) the probe fails without reading, and
/// the passes do the same work one read at a time.
const TAKE: &str =
    "acs_fb=; dd iflag=fullblock count=0 </dev/null >/dev/null 2>&1 && acs_fb=iflag=fullblock; \
     acs_take() { : > \"$2\"; acs_h=0; \
     while [ \"$acs_h\" -lt \"$1\" ]; do \
     dd $acs_fb bs=$(($1 - acs_h)) count=1 2>/dev/null >> \"$2\" || return 1; \
     acs_g=$(($(wc -c < \"$2\"))); \
     [ \"$acs_g\" -gt \"$acs_h\" ] || \
     { echo 'acs: the connection ended before the install was all there' >&2; return 1; }; \
     acs_h=$acs_g; done; };";

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
    // The remote shell has checked the upload already (acs-4km); these
    // digests are belt and braces, so they stream the file through the
    // hash rather than holding a second copy of it (acs-zgof).
    if f.payloads {
        // Our own file is the slim build; complete it with the blob. What
        // is hashed is what is copied, so nothing can change in between.
        let full = dir.join(format!("acs.full.{}", f.token));
        // 0755 whatever the remote's umask (077 would leave 0700, acs-28b).
        let mut out = crate::sys::create_new_mode(&full, 0o755)
            .map_err(|e| format!("{}: {e}", full.display()))?;
        let done = complete(f, &me, &mut out);
        drop(out);
        if let Err(e) = done.and_then(|()| rename(&full, &final_path)) {
            let _ = std::fs::remove_file(&full);
            return Err(e);
        }
        let _ = std::fs::remove_file(&me);
    } else {
        let got = sha256::file(&me).map_err(|e| e.to_string())?;
        if let Some(want) = &f.sha256 {
            if &sha256::hex(&got) != want {
                let _ = std::fs::remove_file(&me);
                return Err("the uploaded binary is corrupt (checksum mismatch)".into());
            }
        }
        rename(&me, &final_path)?;
    }
    match link_bin(&final_path) {
        // `acscp` beside an `acs` of ours, so this host can be the client
        // for the next hop, for copies too (ACSCP_DESIGN §9).
        Ok(Some(bin)) => {
            if let Err(e) = link_copy_name(&bin) {
                eprintln!("acs: warning: cannot link {e} (`acs cp` works without it)");
            }
        }
        Ok(None) => {}
        // The link is a convenience: nothing in the protocol depends on it
        // (DESIGN §8), and the client finds the binary through the prelude.
        // So it is a warning after the ok, not a failed install (acs-iry).
        Err(e) => eprintln!(
            "acs: warning: cannot link ~/.local/bin/acs: {e} (acs is installed at {})",
            final_path.display()
        ),
    }
    Ok(())
}

/// Write the slim build `me` and then the payload set from stdin into
/// `out`, checking each against its digest; a mismatch removes `me`.
fn complete(f: &Finish, me: &Path, out: &mut std::fs::File) -> Result<(), String> {
    let got = sha256::copy_file(me, out).map_err(|e| e.to_string())?;
    if let Some(want) = &f.slim_sha256 {
        if &sha256::hex(&got) != want {
            let _ = std::fs::remove_file(me);
            return Err("the uploaded binary is corrupt (checksum mismatch)".into());
        }
    }
    let mut blob = Vec::new();
    io::stdin()
        .read_to_end(&mut blob)
        .map_err(|e| e.to_string())?;
    if let Some(want) = &f.blob_sha256 {
        if &sha256::hex(&sha256::digest(&blob)) != want {
            let _ = std::fs::remove_file(me);
            return Err("the payload set does not match its checksum".into());
        }
    }
    if !Payloads::is_well_formed(&blob) {
        let _ = std::fs::remove_file(me);
        return Err("the payload set is corrupt".into());
    }
    out.write_all(&blob).map_err(|e| e.to_string())?;
    out.sync_all().map_err(|e| e.to_string())
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("rename to {}: {e}", to.display()))
}

/// Point `~/.local/bin/acs` at the newest install: a temporary symlink
/// renamed over the old one, so it is never missing. Returns the bin
/// directory when the `acs` there is ours afterwards — linked now, or a
/// link of ours to a newer version — so the caller may put `acscp` beside
/// it; `None` when it is the user's own and was left alone.
fn link_bin(target: &Path) -> Result<Option<PathBuf>, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let bin = crate::layout::user_bin(&home);
    let link = bin.join("acs");
    match judge_link(&link, target, &crate::layout::user_root(&home)) {
        BinLink::Link => {}
        BinLink::Newer => return Ok(Some(bin)),
        BinLink::Theirs => return Ok(None),
    }
    std::fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    crate::sys::symlink_atomic(target, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    Ok(Some(bin))
}

/// What to do with `~/.local/bin/acs` for the version just installed.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BinLink {
    /// Point it at the new version.
    Link,
    /// Leave it: a link of ours to a newer version.
    Newer,
    /// Leave it: a file or link the user put there.
    Theirs,
}

/// Whether `~/.local/bin/acs` (`link`) should point at `target`, the
/// version just installed under `share`: when it is missing, or a link of
/// ours (into `share`) to an older or vanished version. A file or link the
/// user put there is left alone, and a newer version keeps the link.
fn judge_link(link: &Path, target: &Path, share: &Path) -> BinLink {
    let meta = match std::fs::symlink_metadata(link) {
        // Nothing there: link. Anything else wrong with the path (~/.local
        // /bin is a regular file): try, so the failure is said out loud
        // rather than passed over (acs-iry).
        Err(_) => return BinLink::Link,
        Ok(m) => m,
    };
    if !meta.file_type().is_symlink() {
        return BinLink::Theirs;
    }
    let Ok(current) = std::fs::read_link(link) else {
        return BinLink::Theirs;
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
        return BinLink::Theirs;
    }
    if !current_c.exists() {
        return BinLink::Link;
    }
    let version = |p: &Path| {
        p.parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .map(str::to_string)
    };
    let forward = match (version(&current_c), version(target)) {
        (Some(have), Some(new)) => crate::release::compare(&new, &have).map_or(true, |o| o.is_ge()),
        _ => true,
    };
    if forward {
        BinLink::Link
    } else {
        BinLink::Newer
    }
}

/// What [`link_copy_name`] found at `dir/acscp`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CopyLink {
    /// Nothing was there: the link was made.
    Made,
    /// Our link was already there; it follows `acs` by itself.
    Kept,
    /// A file or another link the user put there, left alone.
    Theirs,
}

/// Put `dir/acscp -> acs` beside an `acs` of ours (ACSCP_DESIGN §9): the
/// link is relative, so whatever repoints `acs` — `acs upgrade`, an
/// `install.sh` re-run, a newer remote install — moves `acscp` with it. It
/// is made only where nothing is; a link whose target is the bare name
/// `acs` is ours and kept; anything else is the user's and left alone, as
/// `~/.local/bin/acs` is (acs-feee). Shared by the remote install's
/// finisher and `acs upgrade`; the error names the path.
pub(crate) fn link_copy_name(dir: &Path) -> Result<CopyLink, String> {
    let link = dir.join(crate::layout::COPY_NAME);
    match std::fs::symlink_metadata(&link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", link.display())),
        Ok(m) if m.file_type().is_symlink() => {
            let ours = std::fs::read_link(&link).is_ok_and(|t| t == Path::new("acs"));
            return Ok(if ours {
                CopyLink::Kept
            } else {
                CopyLink::Theirs
            });
        }
        Ok(_) => return Ok(CopyLink::Theirs),
    }
    crate::sys::symlink_atomic(Path::new("acs"), &link)
        .map_err(|e| format!("{}: {e}", link.display()))?;
    Ok(CopyLink::Made)
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

    /// acs-5em: the installer in the "not installed" message is the one
    /// beside this build's releases, so a fork built with its own
    /// `ACS_DEFAULT_RELEASES_URL` does not send its users upstream.
    #[test]
    fn the_one_line_installer_follows_this_builds_releases() {
        assert_eq!(
            installer(),
            format!(
                "{}/latest/download/install.sh",
                crate::release::DEFAULT_RELEASES_URL
            )
        );
        // An upstream build: the URL that has always been printed.
        if crate::release::DEFAULT_RELEASES_URL == crate::release::UPSTREAM_RELEASES_URL {
            assert_eq!(
                installer(),
                "https://github.com/michel-onstein/acs/releases/latest/download/install.sh"
            );
        }
    }

    #[test]
    fn same_target_is_a_self_copy() {
        let p = plan("aarch64-apple-darwin", "aarch64-apple-darwin", None).unwrap();
        assert_eq!(p, Plan::SelfCopy);
        // Payloads or not: our own target is always our own file.
        let carried = blob_for("aarch64-apple-darwin", b"slim");
        let p = plan(
            "aarch64-apple-darwin",
            "aarch64-apple-darwin",
            Some(&carried),
        );
        assert_eq!(p.unwrap(), Plan::SelfCopy);
    }

    #[test]
    fn other_linux_target_uses_the_payload() {
        let carried = blob_for("aarch64-unknown-linux-musl", b"slim-arm");
        match plan(
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
            Some(&carried),
        )
        .unwrap()
        {
            Plan::Payload { gz, blob, digest } => {
                assert_eq!(gz, b"GZ");
                assert_eq!(blob, carried.blob());
                // acs-zgof: borrowed from the payloads, not copies of them.
                assert_eq!(blob.as_ptr(), carried.blob().as_ptr());
                assert!(carried.blob().as_ptr_range().contains(&gz.as_ptr()));
                assert_eq!(digest, sha256::digest(b"slim-arm"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_payload_explains_how_to_get_one() {
        let e = plan("x86_64-unknown-linux-musl", "aarch64-apple-darwin", None).unwrap_err();
        assert!(e.contains("cargo xtask dist"), "{e}");
        let e = plan("x86_64-apple-darwin", "aarch64-apple-darwin", None).unwrap_err();
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

        let judge = || judge_link(&link, &new, &share);
        assert_eq!(judge(), BinLink::Link, "missing");
        set(&version("0.1.9"));
        assert_eq!(judge(), BinLink::Link, "older");
        set(&new);
        assert_eq!(judge(), BinLink::Link, "same");
        set(&version("0.10.0"));
        assert_eq!(judge(), BinLink::Newer, "newer stays");
        set(&share.join("0.0.1/acs"));
        assert_eq!(judge(), BinLink::Link, "dangling, pruned");
        set(Path::new("/bin/sh"));
        assert_eq!(judge(), BinLink::Theirs, "someone else's link");
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "hand-installed").unwrap();
        assert_eq!(judge(), BinLink::Theirs, "a regular file");
    }

    /// acs-hofu.5: `acscp` is a relative link to the `acs` beside it, made
    /// where nothing is, kept when it is ours, and never over the user's.
    #[test]
    fn the_copy_name_links_to_the_acs_beside_it() {
        let d = crate::testutil::TempDir::new();
        let bin = d.path();
        let link = bin.join(crate::layout::COPY_NAME);
        std::fs::write(bin.join("acs"), "acs").unwrap();

        assert_eq!(link_copy_name(bin), Ok(CopyLink::Made));
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("acs"));
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "acs");
        // A re-run keeps it, and it follows `acs` wherever that moves.
        assert_eq!(link_copy_name(bin), Ok(CopyLink::Kept));
        std::fs::write(bin.join("acs"), "newer").unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "newer");

        // Another link, even to this acs by its full path, is the user's.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(bin.join("acs"), &link).unwrap();
        assert_eq!(link_copy_name(bin), Ok(CopyLink::Theirs));
        assert_eq!(std::fs::read_link(&link).unwrap(), bin.join("acs"));
        // So is a file.
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "mine").unwrap();
        assert_eq!(link_copy_name(bin), Ok(CopyLink::Theirs));
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "mine");
        // A directory that cannot take it is an error naming the path.
        let err = link_copy_name(&bin.join("no/such/dir")).unwrap_err();
        assert!(err.contains("no/such/dir/acscp"), "{err}");
    }

    #[test]
    fn finish_arguments() {
        let a = |v: &[&str]| parse_finish(&v.iter().map(OsString::from).collect::<Vec<_>>());
        assert!(a(&["--finish", "--token", "abc123", "--sha256", "00"]).is_ok());
        assert!(a(&["--finish", "--token", "../x"]).is_err());
        assert!(a(&["--token", "ab"]).is_err());
    }
}
