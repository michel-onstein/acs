//! `acscp` with no host in it (ACSCP_DESIGN §11): the sender and the
//! receiver in one process over a socketpair, run as the binary under the
//! `acscp` name, and the protocol's pipelining over a slow link.
// The copy is in debug builds only until phase 2 (ACSCP_DESIGN §10.4).
#![cfg(debug_assertions)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use acs::testutil::TempDir;
use common::*;

/// `acs` under the name `acscp`, which is what selects the copy.
struct Acscp {
    _dir: TempDir,
    exe: PathBuf,
}

impl Acscp {
    fn new() -> Acscp {
        let dir = TempDir::new();
        let link = dir.path().join("acscp");
        std::os::unix::fs::symlink(exe(), &link).unwrap();
        Acscp {
            _dir: dir,
            exe: link,
        }
    }

    fn cmd(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = acs_cmd_as(&self.exe);
        c.current_dir(cwd).args(args);
        c
    }
}

fn run(c: &mut Command) -> (i32, String) {
    let out = output_of(c);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// What a tree holds: each path under the root, a directory as `None` and
/// a file as its bytes. Symlinks are not expected in a copy.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            let m = std::fs::symlink_metadata(&p).unwrap();
            assert!(
                !m.file_type().is_symlink(),
                "a symlink in the copy: {rel:?}"
            );
            if m.is_dir() {
                out.insert(rel, None);
                walk(root, &p, out);
            } else {
                out.insert(rel, Some(std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Bytes that are not all one value, so a chunk out of place shows.
fn body(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32).wrapping_mul(2654435761).to_be_bytes()[0] ^ seed)
        .collect()
}

const SIZES: [usize; 6] = [
    0,
    1,
    32 * 1024 - 1,
    32 * 1024,
    32 * 1024 + 1,
    5 * 1024 * 1024,
];

fn root() -> bool {
    acs::sys::geteuid() == 0
}

/// The listed sizes, a nested directory with `-r`, a symlink to a file, a
/// dangling symlink, a fifo and an unreadable file: the copy is the tree
/// byte for byte, less what cannot be copied, which is reported, and the
/// exit status says something was skipped.
#[test]
fn a_tree_is_copied_byte_for_byte() {
    let t = TempDir::new();
    let src = t.path().join("src");
    std::fs::create_dir_all(src.join("nested/deeper")).unwrap();
    let mut want = BTreeMap::new();
    want.insert(PathBuf::new().join("nested"), None);
    want.insert(PathBuf::from("nested/deeper"), None);
    for (i, n) in SIZES.iter().enumerate() {
        let b = body(*n, i as u8);
        std::fs::write(src.join(format!("f{n}")), &b).unwrap();
        std::fs::write(src.join(format!("nested/deeper/g{n}")), &b).unwrap();
        want.insert(PathBuf::from(format!("f{n}")), Some(b.clone()));
        want.insert(PathBuf::from(format!("nested/deeper/g{n}")), Some(b));
    }
    std::fs::write(t.path().join("outside"), "pointed at").unwrap();
    std::os::unix::fs::symlink(t.path().join("outside"), src.join("link")).unwrap();
    want.insert(PathBuf::from("link"), Some(b"pointed at".to_vec()));
    std::os::unix::fs::symlink(t.path().join("nowhere"), src.join("dangling")).unwrap();
    make_fifo(&src.join("fifo"));
    let secret = src.join("nested/secret");
    std::fs::write(&secret, "no").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    if root() {
        // Root reads it anyway.
        want.insert(PathBuf::from("nested/secret"), Some(b"no".to_vec()));
    }

    let cp = Acscp::new();
    let (status, err) = run(&mut cp.cmd(t.path(), &["-r", "src", "dst"]));
    assert_eq!(status, 1, "{err}");
    assert_eq!(tree(&t.path().join("dst")), want);
    assert!(err.contains("dangling: dangling symlink"), "{err}");
    assert!(err.contains("fifo: not a regular file"), "{err}");
    if !root() {
        assert!(err.contains("secret: Permission denied"), "{err}");
    }
    assert!(err.lines().all(|l| l.starts_with("acscp: ")), "{err}");

    // Into an existing directory: under its own name, the same tree.
    std::fs::create_dir(t.path().join("into")).unwrap();
    let (status, _) = run(&mut cp.cmd(t.path(), &["-rq", "src", "into"]));
    assert_eq!(status, 1);
    assert_eq!(tree(&t.path().join("into/src")), want);
}

/// A clean copy exits 0, leaves no part behind and says nothing.
#[test]
fn a_clean_copy_is_quiet_and_exits_zero() {
    let t = TempDir::new();
    std::fs::write(t.path().join("a"), body(100_000, 1)).unwrap();
    std::fs::write(t.path().join("b"), "b").unwrap();
    std::fs::create_dir(t.path().join("to")).unwrap();
    let cp = Acscp::new();
    let (status, err) = run(&mut cp.cmd(t.path(), &["a", "b", "to"]));
    assert_eq!((status, err.as_str()), (0, ""));
    let got = tree(&t.path().join("to"));
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[Path::new("a")].as_deref(), Some(&body(100_000, 1)[..]));

    // One file to a new name, then over it.
    let (status, _) = run(&mut cp.cmd(t.path(), &["a", "c"]));
    assert_eq!(status, 0);
    let (status, _) = run(&mut cp.cmd(t.path(), &["b", "c"]));
    assert_eq!(status, 0);
    assert_eq!(std::fs::read(t.path().join("c")).unwrap(), b"b");
    let parts: Vec<_> = std::fs::read_dir(t.path())
        .unwrap()
        .filter_map(|e| e.unwrap().file_name().into_string().ok())
        .filter(|n| n.ends_with(".acscp-part"))
        .collect();
    assert!(parts.is_empty(), "{parts:?}");

    // -v: one summary line.
    let (status, err) = run(&mut cp.cmd(t.path(), &["-v", "a", "b", "to"]));
    assert_eq!(status, 0);
    assert!(err.contains("2 written, 0 failed, 0 skipped"), "{err}");
}

/// `-p`: the exact mode and both times, to the nanosecond the file system
/// keeps, for files and directories. Without it a file keeps its mode
/// less the umask and gets new times.
#[test]
fn p_preserves_modes_and_times() {
    let t = TempDir::new();
    let src = t.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    let f = src.join("sub/f");
    std::fs::write(&f, "x").unwrap();
    std::fs::write(src.join("run"), "#!/bin/sh\n").unwrap();
    let past = SystemTime::UNIX_EPOCH + Duration::new(1_234_567_890, 123_456_789);
    let older = SystemTime::UNIX_EPOCH + Duration::new(1_000_000_000, 5);
    let times = std::fs::FileTimes::new()
        .set_modified(past)
        .set_accessed(older);
    for p in [&f, &src.join("run")] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_times(times)
            .unwrap();
    }
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::set_permissions(src.join("run"), std::fs::Permissions::from_mode(0o751)).unwrap();
    std::fs::set_permissions(src.join("sub"), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::File::open(src.join("sub"))
        .unwrap()
        .set_times(times)
        .unwrap();

    let cp = Acscp::new();
    let (status, err) = run(&mut cp.cmd(t.path(), &["-rp", "src", "kept"]));
    assert_eq!(status, 0, "{err}");
    // Against the times set, not the source's now: reading it for the
    // copy may have moved its atime.
    for (rel, mode) in [("sub/f", 0o640), ("run", 0o751), ("sub", 0o750)] {
        let b = std::fs::metadata(t.path().join("kept").join(rel)).unwrap();
        assert_eq!(b.mode() & 0o7777, mode, "{rel}");
        assert_eq!(
            (b.mtime(), b.mtime_nsec()),
            (1_234_567_890, 123_456_789),
            "{rel}"
        );
        assert_eq!((b.atime(), b.atime_nsec()), (1_000_000_000, 5), "{rel}");
    }

    // Without -p, under a umask of 027.
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(r#"umask 027 && exec "$0" -r src plain"#)
        .arg(&cp.exe)
        .current_dir(t.path())
        .env("ACS_NO_UPDATE_CHECK", "1");
    let (status, err) = run(&mut c);
    assert_eq!(status, 0, "{err}");
    let mode = |rel: &str| {
        std::fs::metadata(t.path().join("plain").join(rel))
            .unwrap()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("run"), 0o750);
    assert_eq!(mode("sub/f"), 0o640);
    assert_eq!(mode("sub"), 0o750);
    let m = std::fs::metadata(t.path().join("plain/run")).unwrap();
    assert_ne!(m.mtime(), 1_234_567_890);
}

/// ACSCP_DESIGN §4.2 and §6: several sources need a directory, a
/// directory needs -r, a missing source is reported and the rest copied.
#[test]
fn what_is_checked_before_copying() {
    let t = TempDir::new();
    std::fs::write(t.path().join("a"), "a").unwrap();
    std::fs::write(t.path().join("b"), "b").unwrap();
    std::fs::create_dir(t.path().join("d")).unwrap();
    let cp = Acscp::new();

    let (status, err) = run(&mut cp.cmd(t.path(), &["a", "b", "nodir"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("nodir: not a directory"), "{err}");
    assert!(!t.path().join("nodir").exists());

    std::fs::create_dir(t.path().join("to")).unwrap();
    let (status, err) = run(&mut cp.cmd(t.path(), &["d", "missing", "a", "to"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("d: is a directory (use -r)"), "{err}");
    assert!(err.contains("missing: No such file or directory"), "{err}");
    assert_eq!(std::fs::read(t.path().join("to/a")).unwrap(), b"a");
    assert!(!t.path().join("to/d").exists());

    // Nothing left to copy.
    let (status, _) = run(&mut cp.cmd(t.path(), &["missing", "to"]));
    assert_eq!(status, 1);

    // A file over a directory's place, inside the target.
    std::fs::create_dir(t.path().join("to/b")).unwrap();
    let (status, err) = run(&mut cp.cmd(t.path(), &["b", "to"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("is a directory"), "{err}");

    // The command line itself.
    let (status, err) = run(&mut cp.cmd(t.path(), &["a"]));
    assert_eq!(status, 2, "{err}");
    assert!(err.contains("usage: acscp"), "{err}");
    let (status, _) = run(&mut cp.cmd(t.path(), &["-x", "a", "b"]));
    assert_eq!(status, 2);
}

/// A host in the arguments is refused until the remote end exists
/// (phase 2), before anything is dialled.
#[test]
fn a_remote_argument_is_not_built_yet() {
    let t = TempDir::new();
    std::fs::write(t.path().join("a"), "a").unwrap();
    let cp = Acscp::new();
    let mut c = cp.cmd(t.path(), &["a", "box:dst"]);
    c.env("ACS_SSH", "/nonexistent/ssh");
    let (status, err) = run(&mut c);
    assert_eq!(status, 2, "{err}");
    assert!(err.contains("not built yet"), "{err}");
    // ./a:b is a local file.
    std::fs::write(t.path().join("a:b"), "colon").unwrap();
    let (status, err) = run(&mut cp.cmd(t.path(), &["./a:b", "c"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(t.path().join("c")).unwrap(), b"colon");
}

/// `acs cp` is the same role without the second name, and `acscp
/// --version` is `acs --version`: the same file.
#[test]
fn acs_cp_and_the_version() {
    let t = TempDir::new();
    std::fs::write(t.path().join("a"), "a").unwrap();
    let (status, err) = run(acs_cmd().current_dir(t.path()).args(["cp", "a", "b"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(t.path().join("b")).unwrap(), b"a");

    let cp = Acscp::new();
    let ours = output_of(&mut cp.cmd(t.path(), &["--version"]));
    let theirs = output_of(acs_cmd().arg("--version"));
    assert!(ours.status.success());
    assert_eq!(ours.stdout, theirs.stdout);
    let help = output_of(&mut cp.cmd(t.path(), &["--help"]));
    assert!(String::from_utf8_lossy(&help.stdout).contains("-P <port>"));
}

/// Relay `from` to `to`, holding each read for `delay` before writing it:
/// a link with latency and no bandwidth limit. Ends `to`'s writing when
/// `from` ends.
fn delayed(mut from: UnixStream, mut to: UnixStream, delay: Duration) {
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
    std::thread::spawn(move || {
        let mut buf = vec![0; 64 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send((Instant::now() + delay, buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        for (due, bytes) in rx {
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            if to.write_all(&bytes).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    });
}

/// ACSCP_DESIGN §5.3: a hundred one-byte files over a link that delays
/// each direction by 50 ms finish in well under a hundred round trips,
/// since the headers run ahead of the data and only the first file waits
/// for its GO.
#[test]
fn small_files_stream_rather_than_pay_a_round_trip_each() {
    let t = TempDir::new();
    let src = t.path().join("many");
    std::fs::create_dir(&src).unwrap();
    for i in 0..100 {
        std::fs::write(src.join(format!("f{i:03}")), [i as u8]).unwrap();
    }
    let dst = t.path().join("out");

    let delay = Duration::from_millis(50);
    let (sender_end, relay_a) = UnixStream::pair().unwrap();
    let (relay_b, receiver_end) = UnixStream::pair().unwrap();
    delayed(
        relay_a.try_clone().unwrap(),
        relay_b.try_clone().unwrap(),
        delay,
    );
    delayed(relay_b, relay_a, delay);

    let opts = acs::cp::Options::new(true, false);
    let mut quiet = opts.clone();
    quiet.report = false;
    let rx = std::thread::spawn(move || acs::cp::receive(&receiver_end, &quiet));
    let started = Instant::now();
    let sent = acs::cp::send(
        &sender_end,
        &dst.to_string_lossy(),
        vec![("many".into(), src)],
        &opts,
    );
    let took = started.elapsed();
    drop(sender_end);
    let got = rx.join().unwrap();

    assert_eq!(sent.error, None);
    assert_eq!((sent.written, sent.failed, sent.skipped), (100, 0, 0));
    assert_eq!(got.written, 100);
    for i in 0..100 {
        assert_eq!(
            std::fs::read(dst.join(format!("f{i:03}"))).unwrap(),
            [i as u8]
        );
    }
    // A round trip is 100 ms: one per file would be ten seconds. The
    // handshake, the first GO and END/DONE are three; allow for a slow
    // machine, still far from a hundred.
    let rtt = 2 * delay;
    assert!(
        took < rtt * 20,
        "took {took:?}, {} round trips",
        took.as_millis() / rtt.as_millis()
    );
}
