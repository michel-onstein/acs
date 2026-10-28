//! `acscp` with a host in it (ACSCP_DESIGN §8, §11): uploads and downloads
//! against a fake remote — `--transport-cmd` with `common::Remote`, whose
//! prelude execs the real `acs _cp` — first contact installing acs on the
//! one connection, a far end that sends names it should not, and resume
//! (§7): a link cut mid-file, a part that is not the file's start, and a
//! Ctrl-C'd copy finished by the next run. A copy between two hosts runs
//! against two fake remotes behind a fake `ssh` (§5.6).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use acs::cp::proto::{Meta, Msg};
use acs::testutil::TempDir;
use common::*;

/// `acs` under the name `acscp`, run against `remote` from `cwd`.
struct Acscp {
    dir: TempDir,
    exe: PathBuf,
}

impl Acscp {
    fn new() -> Acscp {
        let dir = TempDir::new();
        let link = dir.path().join("acscp");
        std::os::unix::fs::symlink(exe(), &link).unwrap();
        Acscp { dir, exe: link }
    }

    /// A command copying through `transport` (the fake ssh).
    fn cmd(&self, transport: &str, args: &[&str]) -> Command {
        let mut c = acs_cmd_as(&self.exe);
        c.current_dir(self.dir.path())
            .arg("--transport-cmd")
            .arg(transport)
            .args(args);
        c
    }

    fn local(&self) -> &Path {
        self.dir.path()
    }
}

fn run(c: &mut Command) -> (i32, String) {
    let out = output_of(c);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Bytes that are not all one value, so a chunk out of place shows.
fn body(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32).wrapping_mul(2654435761).to_be_bytes()[0] ^ seed)
        .collect()
}

/// Several local files to one directory on the host, and one file under a
/// new name, each over a connection of its own and in one.
#[test]
fn uploads_land_on_the_host() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    let big = body(5 * 1024 * 1024 + 7, 1);
    std::fs::write(cp.local().join("a"), b"alpha").unwrap();
    std::fs::write(cp.local().join("big"), &big).unwrap();
    std::fs::create_dir(remote.home().join("dst")).unwrap();

    let base = remote.connections();
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["a", "big", "devbox:dst"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        remote.connections(),
        base + 1,
        "several sources, one connection"
    );
    assert_eq!(
        std::fs::read(remote.home().join("dst/a")).unwrap(),
        b"alpha"
    );
    assert!(std::fs::read(remote.home().join("dst/big")).unwrap() == big);

    // One file to a new name, relative to the home directory; and to the
    // home directory itself.
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["a", "devbox:renamed"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        std::fs::read(remote.home().join("renamed")).unwrap(),
        b"alpha"
    );
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["a", "devbox:"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(remote.home().join("a")).unwrap(), b"alpha");
    // No part file is left behind anywhere.
    let names: Vec<_> = std::fs::read_dir(remote.home().join("dst"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

/// A tree with `-r`, and several sources on one host into a directory,
/// over one connection; `~/` and absolute paths are the host's.
#[test]
fn downloads_land_here() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    let tree = remote.home().join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("one"), b"1").unwrap();
    std::fs::write(tree.join("sub/two"), body(70_000, 2)).unwrap();
    std::fs::write(remote.home().join("notes.txt"), b"notes").unwrap();
    let abs = remote.root.path().join("elsewhere.txt");
    std::fs::write(&abs, b"absolute").unwrap();
    let out = cp.local().join("out");
    std::fs::create_dir(&out).unwrap();

    let base = remote.connections();
    let abs_arg = format!("devbox:{}", abs.display());
    let (status, err) = run(&mut cp.cmd(
        &remote.transport(),
        &["-r", "devbox:tree", "devbox:~/notes.txt", &abs_arg, "out"],
    ));
    assert_eq!(status, 0, "{err}");
    assert_eq!(
        remote.connections(),
        base + 1,
        "several sources, one connection"
    );
    assert_eq!(std::fs::read(out.join("tree/one")).unwrap(), b"1");
    assert!(std::fs::read(out.join("tree/sub/two")).unwrap() == body(70_000, 2));
    assert_eq!(std::fs::read(out.join("notes.txt")).unwrap(), b"notes");
    assert_eq!(
        std::fs::read(out.join("elsewhere.txt")).unwrap(),
        b"absolute"
    );

    // One file to a new local name.
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["devbox:notes.txt", "mine"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(cp.local().join("mine")).unwrap(), b"notes");

    // What the host cannot read is reported here and skipped, the rest
    // copied, and the status says so.
    let (status, err) = run(&mut cp.cmd(
        &remote.transport(),
        &["devbox:missing", "devbox:notes.txt", "out"],
    ));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("acscp: missing: "), "{err}");

    // Several sources into a local file is refused before any dial.
    let base = remote.connections();
    let (status, err) = run(&mut cp.cmd(
        &remote.transport(),
        &["devbox:notes.txt", "devbox:tree", "mine"],
    ));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("not a directory"), "{err}");
    assert_eq!(remote.connections(), base);
}

/// The name a download arrives under must be known before the far end
/// says it: the home directory, `..` or `/` are refused, not dialled.
#[test]
fn a_download_needs_a_name_to_land_under() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    for src in [
        "devbox:",
        "devbox:~",
        "devbox:~/",
        "devbox:a/..",
        "devbox:/",
    ] {
        let (status, err) = run(&mut cp.cmd(&remote.transport(), &["-r", src, "out"]));
        assert_eq!(status, 2, "{src}: {err}");
        assert!(err.contains("name the file or directory"), "{src}: {err}");
    }
    assert_eq!(remote.connections(), 0);
}

fn leftovers(remote: &Remote) -> Vec<String> {
    let dir = remote.installed_binary(acs::VERSION);
    std::fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n != "acs")
        .collect()
}

/// The first `acscp` to a host without acs installs it and copies, on the
/// one connection, as a session's first contact does
/// (`tests/one_connection_install.rs`).
#[test]
fn first_contact_installs_acs_and_copies_on_one_connection() {
    let remote = Remote::new();
    let cp = Acscp::new();
    std::fs::write(cp.local().join("a"), b"alpha").unwrap();
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["a", "devbox:"]));
    assert_eq!(status, 0, "{err}");
    assert!(
        err.contains(&format!("installed acs {} on devbox", acs::VERSION)),
        "{err}"
    );
    assert_eq!(
        remote.connections(),
        1,
        "first contact made more than one connection"
    );
    assert_eq!(std::fs::read(remote.home().join("a")).unwrap(), b"alpha");
    assert_eq!(
        std::fs::read(remote.installed_binary(acs::VERSION)).unwrap(),
        std::fs::read(exe()).unwrap()
    );
    assert!(leftovers(&remote).is_empty(), "{:?}", leftovers(&remote));

    // And from then on the host serves copies with no install.
    let (status, err) = run(&mut cp.cmd(&remote.transport(), &["devbox:a", "back"]));
    assert_eq!(status, 0, "{err}");
    assert!(!err.contains("installed acs"), "{err}");
    assert_eq!(std::fs::read(cp.local().join("back")).unwrap(), b"alpha");
}

/// `install_on_remote: false` refuses as it does for a session, with the
/// session's message and exit code, and installs nothing.
#[test]
fn install_on_remote_false_refuses() {
    let remote = Remote::new();
    let cp = Acscp::new();
    std::fs::write(cp.local().join("a"), b"alpha").unwrap();
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "install_on_remote: false\n");
    let mut c = cp.cmd(&remote.transport(), &["a", "devbox:"]);
    c.envs(env);
    let (status, err) = run(&mut c);
    assert_eq!(status, 254, "{err}");
    assert!(
        err.contains(&format!(
            "acs {} is not installed on devbox (",
            acs::VERSION
        )),
        "{err}"
    );
    assert!(err.contains("install_on_remote is false"), "{err}");
    assert!(!remote.installed_binary(acs::VERSION).exists());
    assert!(!remote.home().join("a").exists());
}

/// A hostile far end: a transport that answers any `HELLO` with `frames`
/// after the marker, then reads and drops whatever comes, and ends its
/// output once `frames` are out.
fn hostile(dir: &Path, frames: &[Msg]) -> String {
    let mut bytes = acs::proto::ready_line().into_bytes();
    for m in frames {
        m.encode(&mut bytes);
    }
    let file = dir.join("frames");
    std::fs::write(&file, bytes).unwrap();
    let script = dir.join("hostile.sh");
    write_executable(
        &script,
        format!(
            "#!/bin/sh\ncat '{}' &\nexec cat >/dev/null\n",
            file.display()
        ),
    );
    script.display().to_string()
}

fn file(id: u32, name: &str, body: &'static [u8]) -> [Msg<'static>; 3] {
    [
        Msg::File {
            id,
            name: name.into(),
            size: body.len() as u64,
            meta: Meta {
                mode: 0o644,
                ..Meta::default()
            },
        },
        Msg::Data { id, bytes: body },
        Msg::FileEnd { id },
    ]
}

fn welcome() -> Msg<'static> {
    Msg::Welcome {
        umask: 0o022,
        target_is_dir: false,
    }
}

/// CVE-2019-6111's lesson, on the download side: the receiver here
/// accepts only the names it asked for, one component each. A `/` or a
/// `..` in a name ends the copy; a top-level name not asked for — a
/// `.bashrc` in answer to `host:notes.txt` — is refused and reported, and
/// only what was asked for lands.
#[test]
fn hostile_names_from_the_far_end_are_refused() {
    let t = TempDir::new();
    let cp = Acscp::new();
    let out = cp.local().join("out");
    std::fs::create_dir(&out).unwrap();

    for bad in ["../escaped", "/etc/x", "..", "a/b"] {
        let mut frames = vec![welcome()];
        frames.extend(file(1, bad, b"evil"));
        frames.push(Msg::End);
        let transport = hostile(t.path(), &frames);
        let (status, err) = run(&mut cp.cmd(&transport, &["devbox:notes.txt", "out"]));
        assert_eq!(status, 1, "{bad}: {err}");
        assert!(err.contains("protocol error"), "{bad}: {err}");
        assert!(!cp.local().join("escaped").exists());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "{bad}");
    }

    let mut frames = vec![welcome()];
    frames.extend(file(1, ".bashrc", b"evil"));
    frames.extend(file(2, "other", b"evil"));
    frames.extend(file(3, "notes.txt", b"notes"));
    frames.push(Msg::End);
    let transport = hostile(t.path(), &frames);
    let (status, err) = run(&mut cp.cmd(&transport, &["devbox:notes.txt", "out"]));
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains(".bashrc: refused: not a name that was asked for"),
        "{err}"
    );
    assert!(err.contains("other: refused"), "{err}");
    let names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["notes.txt"], "{names:?}");
    assert_eq!(std::fs::read(out.join("notes.txt")).unwrap(), b"notes");
}

/// Local and remote sources together, and downloads from several hosts,
/// are not built: refused before anything is dialled.
#[test]
fn mixed_sources_are_not_built_yet() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    std::fs::write(cp.local().join("a"), b"a").unwrap();
    for args in [
        &["devbox:a", "other:b", "."][..],
        &["a", "devbox:b", "."],
        &["a", "devbox:b", "other:c"],
    ] {
        let (status, err) = run(&mut cp.cmd(&remote.transport(), args));
        assert_eq!(status, 2, "{args:?}: {err}");
        assert!(err.contains("not built yet"), "{args:?}: {err}");
    }
    assert_eq!(remote.connections(), 0);
}

/// A fake `ssh`, written in `dir`, that runs each destination's transport
/// (`Remote::transport`, or [`hostile`]) and refuses any other.
fn router(dir: &Path, hosts: &[(&str, &str)]) -> String {
    let mut cases = String::new();
    for (dest, transport) in hosts {
        cases.push_str(&format!("    '{dest}') exec '{transport}' \"$1\" ;;\n"));
    }
    let path = dir.join("ssh");
    write_executable(
        &path,
        format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             dest=$2\nshift 2\n\
             case \"$dest\" in\n{cases}esac\n\
             echo \"ssh: connect to host $dest port 22: Connection refused\" >&2\nexit 255\n"
        ),
    );
    path.display().to_string()
}

impl Acscp {
    /// A command copying through the fake `ssh` at `ssh`.
    fn via(&self, ssh: &str, args: &[&str]) -> Command {
        let mut c = acs_cmd_as(&self.exe);
        c.current_dir(self.dir.path())
            .arg("--ssh")
            .arg(ssh)
            .args(args);
        c
    }
}

/// Every file and directory under `dir`, by its path below it, with a
/// file's bytes: two trees are equal when these are.
fn tree(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if p.is_dir() {
                out.push((rel, None));
                walk(base, &p, out);
            } else {
                out.push((rel, Some(std::fs::read(&p).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// A copy between two hosts goes through here, `scp -3`'s way
/// (ACSCP_DESIGN §5.6): a tree and a file from one host to a directory on
/// another arrive equal, over one connection to each and with nothing
/// written here. Sources on several hosts are one relay per host, in
/// turn, and a source may be on the target's host itself.
#[test]
fn a_copy_between_two_hosts_is_relayed_here() {
    let (one, two) = (Remote::installed(), Remote::installed());
    let w = TempDir::new();
    let ssh = router(
        w.path(),
        &[("one", &one.transport()), ("two", &two.transport())],
    );
    let cp = Acscp::new();
    let src = one.home().join("tree");
    std::fs::create_dir_all(src.join("sub/deeper")).unwrap();
    std::fs::write(src.join("a"), b"alpha").unwrap();
    std::fs::write(src.join("empty"), b"").unwrap();
    std::fs::write(src.join("sub/big"), body(5 * 1024 * 1024 + 7, 8)).unwrap();
    std::fs::write(src.join("sub/deeper/c"), body(70_000, 9)).unwrap();
    std::fs::write(one.home().join("notes.txt"), b"notes").unwrap();
    std::fs::create_dir(two.home().join("dst")).unwrap();

    let (b1, b2) = (one.connections(), two.connections());
    let (status, err) = run(&mut cp.via(&ssh, &["-r", "one:tree", "one:notes.txt", "two:dst"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!((one.connections(), two.connections()), (b1 + 1, b2 + 1));
    assert!(
        tree(&src) == tree(&two.home().join("dst/tree")),
        "the trees differ"
    );
    assert_eq!(
        std::fs::read(two.home().join("dst/notes.txt")).unwrap(),
        b"notes"
    );
    let here: Vec<_> = std::fs::read_dir(cp.local()).unwrap().collect();
    assert_eq!(here.len(), 1, "only the acscp link is here: {here:?}");

    // One file to a new name; then sources from both hosts into one
    // directory on the second.
    std::fs::write(two.home().join("mine.txt"), b"mine").unwrap();
    let (status, err) = run(&mut cp.via(&ssh, &["one:notes.txt", "two:renamed"]));
    assert_eq!(status, 0, "{err}");
    assert_eq!(std::fs::read(two.home().join("renamed")).unwrap(), b"notes");
    std::fs::create_dir(two.home().join("both")).unwrap();
    let (status, err) = run(&mut cp.via(
        &ssh,
        &["one:notes.txt", "two:mine.txt", "one:tree/a", "two:both"],
    ));
    assert_eq!(status, 0, "{err}");
    for (name, want) in [
        ("notes.txt", &b"notes"[..]),
        ("mine.txt", b"mine"),
        ("a", b"alpha"),
    ] {
        let got = std::fs::read(two.home().join("both").join(name)).unwrap();
        assert_eq!(got, want, "{name}");
    }

    // What the source host cannot read is reported here; several sources
    // into what is not a directory are refused before any data goes.
    let (status, err) = run(&mut cp.via(&ssh, &["one:missing", "one:notes.txt", "two:dst"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("acscp: missing: "), "{err}");
    let (status, err) = run(&mut cp.via(&ssh, &["one:notes.txt", "two:mine.txt", "two:renamed"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("two:renamed: not a directory"), "{err}");
    assert_eq!(std::fs::read(two.home().join("renamed")).unwrap(), b"notes");
}

/// Two hosts that have never seen acs are each installed on their own
/// connection: the once-per-process guard on an install is per host.
#[test]
fn a_copy_between_two_fresh_hosts_installs_both() {
    let (one, two) = (Remote::new(), Remote::new());
    let w = TempDir::new();
    let ssh = router(
        w.path(),
        &[("one", &one.transport()), ("two", &two.transport())],
    );
    let cp = Acscp::new();
    std::fs::write(one.home().join("a"), b"alpha").unwrap();
    let (status, err) = run(&mut cp.via(&ssh, &["one:a", "two:"]));
    assert_eq!(status, 0, "{err}");
    for host in ["one", "two"] {
        assert!(
            err.contains(&format!("installed acs {} on {host}", acs::VERSION)),
            "{err}"
        );
    }
    assert_eq!((one.connections(), two.connections()), (1, 1));
    assert_eq!(std::fs::read(two.home().join("a")).unwrap(), b"alpha");
}

/// CVE-2019-6111's lesson, between two hosts: a name from the source host
/// that could leave the target ends the copy here, before the target host
/// sees it, and a top-level name not asked for is refused by the target's
/// receiver and reported here. A target host that sends what only a
/// sender may — or a name made to steer the terminal — is broken, not
/// obeyed.
#[test]
fn hostile_names_from_either_host_are_refused() {
    let good = Remote::installed();
    std::fs::write(good.home().join("notes.txt"), b"notes").unwrap();
    std::fs::create_dir(good.home().join("dst")).unwrap();
    let cp = Acscp::new();
    let landed = || -> Vec<_> {
        std::fs::read_dir(good.home().join("dst"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    };

    // From the source host.
    for bad in ["../escaped", "/etc/x", "..", "a/b"] {
        let t = TempDir::new();
        let mut frames = vec![welcome()];
        frames.extend(file(1, bad, b"evil"));
        frames.push(Msg::End);
        let evil = hostile(t.path(), &frames);
        let ssh = router(t.path(), &[("evil", &evil), ("good", &good.transport())]);
        let (status, err) = run(&mut cp.via(&ssh, &["evil:notes.txt", "good:dst"]));
        assert_eq!(status, 1, "{bad}: {err}");
        assert!(err.contains("protocol error from evil"), "{bad}: {err}");
        assert!(!good.home().join("escaped").exists(), "{bad}");
        assert!(landed().is_empty(), "{bad}: {:?}", landed());
    }
    let t = TempDir::new();
    let mut frames = vec![welcome()];
    frames.extend(file(1, ".bashrc", b"evil"));
    frames.extend(file(2, "notes.txt", b"notes"));
    frames.push(Msg::End);
    let evil = hostile(t.path(), &frames);
    let ssh = router(t.path(), &[("evil", &evil), ("good", &good.transport())]);
    let (status, err) = run(&mut cp.via(&ssh, &["evil:notes.txt", "good:dst"]));
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains(".bashrc: refused: not a name that was asked for"),
        "{err}"
    );
    assert_eq!(landed(), ["notes.txt"]);

    // From the target host: a FAIL whose name would steer the terminal,
    // then a FILE or a DIR, which only a sender sends.
    for (bad, what) in [
        (file(1, "x", b"x")[0].clone(), "unexpected message type 7"),
        (
            Msg::Dir {
                name: "../x".into(),
                meta: Meta::default(),
            },
            "protocol error",
        ),
    ] {
        let t = TempDir::new();
        let frames = [
            welcome(),
            Msg::Fail {
                id: 1,
                name: "\x1b]0;pwned\x07x".into(),
                message: "no".into(),
            },
            bad,
        ];
        let evil = hostile(t.path(), &frames);
        let ssh = router(t.path(), &[("good", &good.transport()), ("evil", &evil)]);
        let (status, err) = run(&mut cp.via(&ssh, &["good:notes.txt", "evil:dst"]));
        assert_eq!(status, 1, "{err}");
        assert!(err.contains(what) && err.contains("from evil"), "{err}");
        assert!(!err.contains('\x1b') && !err.contains('\x07'), "{err:?}");
    }
}

/// A lost link ends a copy between two hosts and says so — the relay has
/// nothing to resume from (ACSCP_DESIGN §5.6) — and the part it leaves on
/// the target host is what the same command run again goes on from.
#[test]
fn a_lost_link_ends_a_copy_between_two_hosts() {
    let (one, two) = (Remote::installed(), Remote::installed());
    let size = 8 << 20;
    let big = body(size, 10);
    std::fs::write(one.home().join("big"), &big).unwrap();
    let w = TempDir::new();
    let (a, b) = cut(w.path(), false);
    let cutting = w.path().join("cutting.sh");
    write_executable(
        &cutting,
        format!(
            "#!/bin/sh\nexec 3<&0; '{real}' \"$1\" <&3 >'{a}' & exec cat '{b}'\n",
            real = one.transport(),
            a = a.display(),
            b = b.display()
        ),
    );
    let cutting = cutting.display().to_string();
    let ssh = router(w.path(), &[("one", &cutting), ("two", &two.transport())]);
    let cp = Acscp::new();
    let (status, err) = run(&mut cp.via(&ssh, &["one:big", "two:"]));
    assert_eq!(status, 1, "{err}");
    assert!(
        err.contains("one: the link closed before the copy finished"),
        "{err}"
    );
    assert!(err.contains("does not reconnect — run it again"), "{err}");
    assert_eq!((one.connections(), two.connections()), (1, 1));
    assert!(!two.home().join("big").exists());
    let part = two.home().join(".big.acscp-part");
    assert!(
        weighed(&part) > CUT / 2,
        "the part holds {}",
        weighed(&part)
    );

    // The second run carries to the target host only what its part lacks.
    let carried = w.path().join("carried");
    let weighing = w.path().join("weighing.sh");
    write_executable(
        &weighing,
        format!(
            "#!/bin/sh\ntee '{}' | '{}' \"$1\"\n",
            carried.display(),
            two.transport()
        ),
    );
    let weighing = weighing.display().to_string();
    let ssh = router(w.path(), &[("one", &one.transport()), ("two", &weighing)]);
    let (status, err) = run(&mut cp.via(&ssh, &["one:big", "two:"]));
    assert_eq!(status, 0, "{err}");
    assert!(std::fs::read(two.home().join("big")).unwrap() == big);
    assert!(!part.exists());
    carried_once(weighed(&carried), size);
}

/// A transport in front of `remote`'s, written in `dir`: the first
/// connection runs `first`, every later one `then`, each a shell pipeline
/// in which `"$REAL" "$1"` is the remote's own transport. `tee` in one is
/// how a test weighs what that connection carried, and [`CUT`] cuts it.
fn wrapped(remote: &Remote, dir: &Path, first: &str, then: &str) -> String {
    let flag = dir.join("first");
    std::fs::write(&flag, "").unwrap();
    let script = dir.join("wrapped.sh");
    write_executable(
        &script,
        format!(
            "#!/bin/sh\nREAL='{real}'\n\
             if [ -f '{flag}' ]; then rm -f '{flag}'; {first}; exit; fi\n\
             {then}\n",
            real = remote.transport(),
            flag = flag.display(),
        ),
    );
    script.display().to_string()
}

/// How many bytes the file at `path` holds: what a `tee` weighed, or a part.
fn weighed(path: &Path) -> usize {
    std::fs::metadata(path).map_or(0, |m| m.len() as usize)
}

/// Where the first connection is cut: well inside the files below.
const CUT: usize = 3_000_000;

/// The cut, made by the test rather than by a shell tool (macOS's `head`
/// and `dd` both hold a short read back until more comes, which kept the
/// handshake's first frames from the far end until it gave up on them):
/// fifos `a` and `b` in `dir`, and a thread passing on the first [`CUT`]
/// bytes written to `a` to `b` the moment they come. Then it closes `b` —
/// the link ends mid-stream — or, `stalled`, goes on reading `a` and drops
/// the rest, as a link that has gone quiet with its host still there.
fn cut(dir: &Path, stalled: bool) -> (PathBuf, PathBuf) {
    use std::io::{Read, Write};
    let (a, b) = (dir.join("a"), dir.join("b"));
    make_fifo(&a);
    make_fifo(&b);
    let (from, to) = (a.clone(), b.clone());
    std::thread::spawn(move || {
        let mut from = std::fs::File::open(&from).unwrap();
        let mut to = std::fs::OpenOptions::new().write(true).open(&to).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = CUT;
        while left > 0 {
            let n = match from.read(&mut buf[..left.min(64 * 1024)]) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if to.write_all(&buf[..n]).is_err() {
                return;
            }
            left -= n;
        }
        if stalled {
            let _ = std::io::copy(&mut from, &mut std::io::sink());
        }
    });
    (a, b)
}

/// The transport ends mid-file — the first connection carries 3 MB of an
/// 8 MB download and ends — and the copy redials at once and finishes
/// (ACSCP_DESIGN §7). The part was reused, not written again: the second
/// connection carried only what the first had not, so the two together
/// carried the file about once.
#[test]
fn a_download_cut_mid_file_resumes_from_its_part() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    let size = 8 << 20;
    let big = body(size, 3);
    std::fs::write(remote.home().join("big"), &big).unwrap();
    let w = TempDir::new();
    let (a, b) = cut(w.path(), false);
    let second = w.path().join("second.tee");
    let transport = wrapped(
        &remote,
        w.path(),
        &format!(
            "exec 3<&0; \"$REAL\" \"$1\" <&3 >'{}' & exec cat '{}'",
            a.display(),
            b.display()
        ),
        &format!("\"$REAL\" \"$1\" | tee '{}'", second.display()),
    );
    let base = remote.connections();
    let mut c = cp.cmd(&transport, &["-v", "devbox:big", "out"]);
    c.envs(FAST.iter().copied());
    let (status, err) = run(&mut c);
    assert_eq!(status, 0, "{err}");
    assert_eq!(remote.connections(), base + 2, "{err}");
    assert!(err.contains("acscp: devbox: "), "{err}");
    assert!(err.contains("reconnecting now"), "{err}");
    assert!(err.contains("out: resuming from the "), "{err}");
    assert!(std::fs::read(cp.local().join("out")).unwrap() == big);
    assert!(!cp.local().join(".out.acscp-part").exists());
    carried_once(weighed(&second), size);
}

/// What the second connection carried of a `size`-byte file whose first
/// carried [`CUT`] bytes: only the rest, so about the file once between
/// them. Without the part it would be all of the file again.
fn carried_once(second: usize, size: usize) {
    let what = format!("the second connection carried {second} bytes of {size}, cut at {CUT}");
    assert!(second > 0 && second < size - CUT / 2, "{what}");
    assert!(CUT + second < size + 128 * 1024, "{what}");
}

/// The same for an upload, the cut on the way to the host: the host's
/// part is offered with `HAVE`, and the sender here hashes its own start
/// and goes on after it.
#[test]
fn an_upload_cut_mid_file_resumes_from_its_part() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    let size = 8 << 20;
    let big = body(size, 4);
    std::fs::write(cp.local().join("big"), &big).unwrap();
    let w = TempDir::new();
    let (a, b) = cut(w.path(), false);
    let second = w.path().join("second.tee");
    let transport = wrapped(
        &remote,
        w.path(),
        &format!(
            "exec 3<&0; cat <&3 >'{}' & exec \"$REAL\" \"$1\" <'{}'",
            a.display(),
            b.display()
        ),
        &format!("tee '{}' | \"$REAL\" \"$1\"", second.display()),
    );
    let mut c = cp.cmd(&transport, &["-v", "big", "devbox:"]);
    c.envs(FAST.iter().copied());
    let (status, err) = run(&mut c);
    assert_eq!(status, 0, "{err}");
    assert!(err.contains("big: resuming after "), "{err}");
    assert!(std::fs::read(remote.home().join("big")).unwrap() == big);
    assert!(!remote.home().join(".big.acscp-part").exists());
    carried_once(weighed(&second), size);
}

/// A part that is not the start of the file it is named for — another
/// file's, or the source changed since — is written again from 0: the
/// connection carries all of the file (ACSCP_DESIGN §7, decision 6).
#[test]
fn a_part_with_a_wrong_start_is_written_again() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    let size = 1 << 20;
    let big = body(size, 5);
    std::fs::write(remote.home().join("big"), &big).unwrap();
    std::fs::write(cp.local().join(".out.acscp-part"), vec![0xee; size / 2]).unwrap();
    let w = TempDir::new();
    let carried = w.path().join("carried");
    let tee = format!("\"$REAL\" \"$1\" | tee '{}'", carried.display());
    let transport = wrapped(&remote, w.path(), &tee, &tee);
    let (status, err) = run(&mut cp.cmd(&transport, &["-v", "devbox:big", "out"]));
    assert_eq!(status, 0, "{err}");
    assert!(
        err.contains("out: the part here is not its start — copying it whole"),
        "{err}"
    );
    assert!(std::fs::read(cp.local().join("out")).unwrap() == big);
    assert!(!cp.local().join(".out.acscp-part").exists());
    assert!(weighed(&carried) >= size, "{}", weighed(&carried));
}

/// Ctrl-C ends a copy at once and leaves its part (ACSCP_DESIGN §5.5);
/// the next run of the same copy finishes from it, carrying only the rest.
/// The part is the only state there is, so nothing else has to survive.
#[test]
fn an_interrupted_copy_is_finished_by_the_next_run() {
    use std::os::unix::process::ExitStatusExt;
    let remote = Remote::installed();
    let cp = Acscp::new();
    let size = 8 << 20;
    let big = body(size, 6);
    std::fs::write(remote.home().join("big"), &big).unwrap();
    let w = TempDir::new();
    let carried = w.path().join("carried");
    // The first run's link goes quiet after the cut, as a stalled one
    // does, with the host still there.
    let (a, b) = cut(w.path(), true);
    let transport = wrapped(
        &remote,
        w.path(),
        &format!(
            "exec 3<&0; \"$REAL\" \"$1\" <&3 >'{}' & exec cat '{}'",
            a.display(),
            b.display()
        ),
        &format!("\"$REAL\" \"$1\" | tee '{}'", carried.display()),
    );
    let part = cp.local().join(".out.acscp-part");
    let mut child = cp
        .cmd(&transport, &["devbox:big", "out"])
        .env("ACS_DEAD_MS", "600000")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        T,
        || weighed(&part) >= 1 << 20,
        || format!("the part holds {} bytes", weighed(&part)),
    );
    acs::sys::kill(child.id() as i32, libc::SIGINT).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
    let kept = weighed(&part);
    assert!(
        (1 << 20..size).contains(&kept),
        "the part holds {kept} bytes"
    );
    assert!(!cp.local().join("out").exists());

    let (status, err) = run(&mut cp.cmd(&transport, &["-v", "devbox:big", "out"]));
    assert_eq!(status, 0, "{err}");
    assert!(
        err.contains(&format!("out: resuming from the {kept} bytes already here")),
        "{err}"
    );
    assert!(std::fs::read(cp.local().join("out")).unwrap() == big);
    assert!(!part.exists());
    let second = weighed(&carried);
    assert!(
        second >= size - kept && second < size - kept + 64 * 1024,
        "the second run carried {second} bytes; {kept} of {size} were here"
    );
}

/// `--no-reconnect` ends on the first lost link, as `scp` would, with the
/// part kept for a later run and no second connection.
#[test]
fn no_reconnect_ends_on_the_first_lost_link() {
    let remote = Remote::installed();
    let cp = Acscp::new();
    std::fs::write(remote.home().join("big"), body(8 << 20, 7)).unwrap();
    let w = TempDir::new();
    let (a, b) = cut(w.path(), false);
    let transport = wrapped(
        &remote,
        w.path(),
        &format!(
            "exec 3<&0; \"$REAL\" \"$1\" <&3 >'{}' & exec cat '{}'",
            a.display(),
            b.display()
        ),
        "exit 99",
    );
    let base = remote.connections();
    let (status, err) = run(&mut cp.cmd(&transport, &["--no-reconnect", "devbox:big", "out"]));
    assert_eq!(status, 1, "{err}");
    assert!(err.contains("went away before the copy finished"), "{err}");
    assert!(!err.contains("reconnecting"), "{err}");
    assert_eq!(remote.connections(), base + 1);
    assert!(weighed(&cp.local().join(".out.acscp-part")) > CUT / 2);
    assert!(!cp.local().join("out").exists());
}
