//! First contact installs acs and starts the session on one ssh connection
//! (acs-6k2, DESIGN §8.1): the prelude says `ACS-NEED`, reads the install
//! from the same stdin, checks it before anything is executable, installs,
//! and execs the proxy on what is left.

mod common;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;

const CMD: &[&str] = &["--", "/bin/sh", "-c", "echo up; sleep 30"];

fn args(session: &str) -> Vec<&str> {
    let mut a = vec!["devbox", session];
    a.extend_from_slice(CMD);
    a
}

fn leftovers(remote: &Remote) -> Vec<String> {
    let dir = remote.installed_binary(acs::VERSION);
    std::fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n != "acs")
        .collect()
}

/// Before acs-6k2 a self-copy took three connections — the dial that got
/// ACS-NEED, the upload, the redial — and a payload install four. Now
/// both take the one dial.
#[test]
fn a_self_copy_install_and_its_session_take_one_connection() {
    let remote = Remote::new();
    let mut c = Client::start(&remote, &args("one"));
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);
    assert_eq!(
        remote.transport_pids().len(),
        1,
        "first contact made more than one connection"
    );
    assert_eq!(
        std::fs::read(remote.installed_binary(acs::VERSION)).unwrap(),
        std::fs::read(exe()).unwrap()
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

#[test]
fn a_payload_install_and_its_session_take_one_connection() {
    let remote = Remote::new();
    remote.fake_uname("Linux", "x86_64");
    let (client, slim, blob) = complete_client(remote.root.path(), "x86_64-unknown-linux-musl");
    let mut c = Client::start_exe(&client, &remote, &args("pay"), &[]);
    c.wait_for("(Linux x86_64)", T);
    c.wait_for("up", T);
    assert_eq!(
        remote.transport_pids().len(),
        1,
        "first contact made more than one connection"
    );
    let mut expected = slim;
    expected.extend_from_slice(&blob);
    assert!(
        std::fs::read(remote.installed_binary(acs::VERSION)).unwrap() == expected,
        "installed binary differs from the complete build"
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

/// Two first contacts racing on one host each install over their own
/// connection, into files of their own, and converge on one binary.
#[test]
fn racing_first_contacts_each_take_one_connection() {
    let remote = Remote::new();
    let mut a = Client::start(&remote, &args("a"));
    let mut b = Client::start(&remote, &args("b"));
    a.wait_for("up", T);
    b.wait_for("up", T);
    assert_eq!(remote.transport_pids().len(), 2);
    assert_eq!(
        std::fs::read(remote.installed_binary(acs::VERSION)).unwrap(),
        std::fs::read(exe()).unwrap()
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
}

/// An upload the remote shell finds does not match fails the client with
/// the remote's own reason and its exit status, and installs nothing.
#[test]
fn a_mismatched_upload_fails_the_client_and_installs_nothing() {
    let remote = Remote::new();
    // The transport puts `fakebin` first on the remote's PATH: a
    // `sha256sum` there that disagrees with every upload.
    let fakebin = remote.root.path().join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    write_executable(
        &fakebin.join("sha256sum"),
        format!("#!/bin/sh\necho {}  \"$1\"\n", "0".repeat(64)),
    );
    let mut c = Client::start(&remote, &args("bad"));
    assert_eq!(c.wait(T), 254);
    c.wait_for("does not match the checksum", T);
    c.wait_for("installing acs on devbox failed (exit status 5)", T);
    assert!(!remote.installed_binary(acs::VERSION).exists());
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));
    assert_eq!(remote.transport_pids().len(), 1);
}

// ---- the prelude and the script, driven the way the remote's sh would ------

const SYNC: &str = "0123456789abcdef";
const TOKEN: &str = "feedface";

/// A stand-in for acs: its `_install --finish` moves it into place (and
/// runs `finish_extra`), and run any other way it says so and echoes what
/// is left of its stdin. Every run leaves a mark in `$HOME/ran`.
fn fake_acs(finish_extra: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\necho \"$*\" >> \"$HOME/ran\"\n\
         if [ \"$1\" = _install ]; then d=$(dirname \"$0\"); mv \"$0\" \"$d/acs\"; {finish_extra}\n echo ok; exit 0; fi\n\
         echo \"RAN $*\"; cat\n"
    )
    .into_bytes()
}

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the installing prelude under `home` with `stdin`, as a session's
/// would be: the HELLO that went ahead with the dial first, then the rest.
fn run_prelude(home: &Path, stdin: Vec<u8>) -> Run {
    run_prelude_with(home, stdin, None)
}

/// [`run_prelude`] with `bin` first on the PATH.
fn run_prelude_with(home: &Path, stdin: Vec<u8>, bin: Option<&Path>) -> Run {
    let mut cmd = Command::new("/bin/sh");
    if let Some(bin) = bin {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{path}", bin.display()));
    }
    let mut child = cmd
        .arg("-c")
        .arg(acs::ssh::prelude_installing(
            acs::VERSION,
            &["--version"],
            SYNC,
        ))
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut to = child.stdin.take().unwrap();
    // A pipe write at a time, so the script's reads meet short ones.
    let w = std::thread::spawn(move || {
        for chunk in stdin.chunks(1000) {
            if to.write_all(chunk).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    let out = child.wait_with_output().unwrap();
    w.join().unwrap();
    Run {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// What a client sends: part of a HELLO with no newline (binary, a NUL in
/// it), the install for `data` claiming `digest`, and what follows it.
fn stream(data: &[u8], len: usize, digest: &str, after: &[u8]) -> Vec<u8> {
    let mut s = b"\x00\x01HELLO-cut\x00short".to_vec();
    let script = acs::install::self_copy_script(TOKEN, len, digest);
    s.extend_from_slice(&acs::install::header(SYNC, &script));
    s.extend_from_slice(data);
    s.extend_from_slice(after);
    s
}

fn version_dir(home: &Path) -> std::path::PathBuf {
    home.join(format!(".local/share/acs/{}", acs::VERSION))
}

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The script takes exactly the bytes it was told to: the binary's, not
/// one more — what follows goes to the acs the prelude execs, which is
/// where the session's HELLO is on a real connection.
#[test]
fn the_install_takes_its_bytes_and_hands_the_rest_to_the_new_acs() {
    take_exactly(false);
}

/// The same where `dd` has no `iflag=fullblock` — the BSDs before macOS
/// grew one, older busybox — so `acs_take` meets every short read the pipe
/// hands it, and loops until it has the lot.
#[test]
fn the_install_takes_its_bytes_one_read_at_a_time_without_fullblock() {
    take_exactly(true);
}

fn take_exactly(without_fullblock: bool) {
    let home = acs::testutil::TempDir::new();
    let shim = acs::testutil::TempDir::new();
    let passes = shim.path().join("passes");
    if without_fullblock {
        let real = String::from_utf8(
            Command::new("/bin/sh")
                .args(["-c", "command -v dd"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        write_executable(
            &shim.path().join("dd"),
            format!(
                "#!/bin/sh
for a; do case $a in iflag=*) echo \"dd: unknown operand $a\" >&2; exit 1;; esac; done
\
                 echo pass >> '{}'\nexec {} \"$@\"\n",
                passes.display(),
                real.trim()
            ),
        );
    }
    // Big enough to need many reads off the pipe.
    let mut bin = fake_acs("");
    bin.extend(vec![b'#'; 300_000]);
    bin.push(b'\n');
    let digest = acs::sha256::hex(&acs::sha256::digest(&bin));
    let r = run_prelude_with(
        home.path(),
        stream(&bin, bin.len(), &digest, b"THE-NEXT-HELLO\n"),
        without_fullblock.then_some(shim.path()),
    );
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    assert!(r.stdout.contains("ACS-NEED"), "{}", r.stdout);
    assert!(r.stdout.contains("RAN --version"), "{}", r.stdout);
    assert!(
        r.stdout.ends_with("THE-NEXT-HELLO\n"),
        "the rest of stdin did not reach the new acs intact: {:?}",
        &r.stdout[r.stdout.len().saturating_sub(80)..]
    );
    let installed = version_dir(home.path()).join("acs");
    assert_eq!(std::fs::read(&installed).unwrap(), bin);
    assert_eq!(files_in(&version_dir(home.path())), ["acs"]);
    if without_fullblock {
        let n = std::fs::read_to_string(&passes).unwrap().lines().count();
        assert!(
            n > 1,
            "the upload came in {n} read(s): the loop was not exercised"
        );
    }
}

/// acs-4km survives the move onto one connection: the shell checks the
/// upload against the client's digest before it is made executable, and a
/// mismatch never runs and leaves nothing behind.
#[test]
fn an_upload_that_does_not_match_its_digest_is_never_run() {
    let home = acs::testutil::TempDir::new();
    let bin = fake_acs("");
    let digest = acs::sha256::hex(&acs::sha256::digest(b"what the client meant"));
    let r = run_prelude(home.path(), stream(&bin, bin.len(), &digest, b""));
    assert_eq!(r.status, Some(5), "{}", r.stderr);
    assert!(
        r.stderr.contains("does not match the checksum"),
        "{}",
        r.stderr
    );
    assert!(!home.path().join("ran").exists(), "the upload ran");
    assert!(!r.stdout.contains("RAN"));
    assert!(
        files_in(&version_dir(home.path())).is_empty(),
        "{:?}",
        files_in(&version_dir(home.path()))
    );
}

/// The install goes through the same checks as any connection before it is
/// run (acs-08m): here the finisher leaves the binary group-writable, so
/// the prelude refuses it and says the install cannot be run.
#[test]
fn an_install_the_prelude_will_not_trust_is_not_run() {
    let home = acs::testutil::TempDir::new();
    let bin = fake_acs("chmod 775 \"$d/acs\"");
    let digest = acs::sha256::hex(&acs::sha256::digest(&bin));
    let r = run_prelude(home.path(), stream(&bin, bin.len(), &digest, b""));
    assert_eq!(
        r.status,
        Some(acs::ssh::INSTALLED_BUT_REFUSED),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("refusing to run"), "{}", r.stderr);
    assert!(r.stderr.contains("is writable by others"), "{}", r.stderr);
    assert!(!r.stdout.contains("RAN"), "{}", r.stdout);
    let installed = version_dir(home.path()).join("acs");
    assert_eq!(
        std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
        0o775
    );
}

/// A connection that ends mid-upload fails the install and leaves no
/// temporary file behind.
#[test]
fn an_upload_cut_short_fails_and_leaves_nothing() {
    let home = acs::testutil::TempDir::new();
    let bin = fake_acs("");
    let digest = acs::sha256::hex(&acs::sha256::digest(&bin));
    let r = run_prelude(
        home.path(),
        stream(&bin[..bin.len() / 2], bin.len(), &digest, b""),
    );
    assert_ne!(r.status, Some(0), "{}", r.stderr);
    assert!(
        r.stderr.contains("ended before the install"),
        "{}",
        r.stderr
    );
    assert!(!home.path().join("ran").exists(), "the upload ran");
    assert!(
        files_in(&version_dir(home.path())).is_empty(),
        "{:?}",
        files_in(&version_dir(home.path()))
    );
}

/// Only the line with this dial's token starts an install: anything else
/// on stdin — a sync line with another token, the script that would follow
/// it — is passed over, and the end of input leaves quietly.
#[test]
fn nothing_but_the_dials_own_sync_line_starts_an_install() {
    let home = acs::testutil::TempDir::new();
    let pwned = home.path().join("pwned");
    let mut s = acs::install::header("ffffffffffffffff", &format!("touch '{}'", pwned.display()));
    s.extend_from_slice(format!("touch '{}'\n", pwned.display()).as_bytes());
    let r = run_prelude(home.path(), s);
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    assert!(r.stdout.contains("ACS-NEED"), "{}", r.stdout);
    assert!(!pwned.exists(), "a line that was not the sync line ran");
}

/// The plain prelude — `acs list`, the session menu, `install_on_remote:
/// false` — still ends at ACS-NEED without reading anything.
#[test]
fn the_plain_prelude_does_not_wait_for_an_install() {
    let home = acs::testutil::TempDir::new();
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(acs::ssh::prelude(acs::VERSION, &["--version"]))
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // stdin stays open: a prelude that read it would never finish.
    let _keep = child.stdin.take().unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ACS-NEED"));
}
