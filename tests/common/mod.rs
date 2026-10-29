//! Integration harness (DESIGN §9.1): a fake remote in a temp HOME, a
//! transport that replaces ssh, and a pty runner for the real client.
#![allow(dead_code)]

use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acs::sys;
use acs::testutil::TempDir;

/// The deadline test helpers wait to; see [`acs::testutil::T`].
pub use acs::testutil::T;
/// The timers a wait to `T` is proved against, derived from it (acs-3sk9),
/// and their spellings as `ACS_*_MS` values and as the client prints them.
/// Not every test binary reads every one.
#[allow(unused_imports)]
pub use acs::testutil::{ms, secs, OUTLAST_ONE_WAIT, OUTLAST_TWO_WAITS};
/// Deadline waits as free functions, for any condition (acs-bndl).
pub use acs::testutil::{poll, wait_until};

/// A directory that does not exist: the client finds no configuration file
/// unless a test gives it one.
pub const NO_CONFIG: &str = "/nonexistent/acs-test-config";

/// A state directory that cannot exist, even for root (`/dev/null/…` is not
/// a directory): the client remembers no alias's last host (acs-zpj), so
/// one test's choice cannot steer another's, nor the developer's own. A
/// test about the memory passes its own `XDG_STATE_HOME`.
pub const NO_STATE: &str = "/dev/null";

/// `acs` as a plain command (no pty), isolated from the developer's own
/// configuration.
pub fn acs_cmd() -> Command {
    acs_cmd_as(&exe())
}

/// [`acs_cmd`] running the copy of acs at `path`.
pub fn acs_cmd_as(path: &Path) -> Command {
    let mut c = Command::new(path);
    c.env("ACS_NO_UPDATE_CHECK", "1")
        .env("XDG_CONFIG_HOME", NO_CONFIG)
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
        .env("XDG_STATE_HOME", NO_STATE)
        .env("ACS_CONTROL_PERSIST", NO_MASTER);
    c
}

/// No shared ssh master by default (acs-9n3): a test that is not about one
/// must not make, or join, a master in the developer's own
/// `/tmp/acs-mux-<uid>`. `tests/mux.rs` sets its own directory and window,
/// applied after this one.
pub const NO_MASTER: &str = "0";

/// Write `yaml` as the client's local configuration under `dir`; returns the
/// environment that makes the client read it.
pub fn config_env(dir: &Path, yaml: &str) -> Vec<(String, String)> {
    let file = dir.join("acs/config.yaml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, yaml).unwrap();
    vec![("XDG_CONFIG_HOME".into(), dir.display().to_string())]
}

/// A named pipe at `path`, for a test that has to let a remote program
/// past a point of its own choosing rather than after a sleep.
pub fn make_fifo(path: &Path) {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: a path in the test's own temporary directory.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// Let a reader of the fifo at `path` past: it blocks until the program
/// has opened it, so the release is ordered against what the program does
/// next and not against the clock.
pub fn release_fifo(path: &Path) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.write_all(b"x").unwrap();
}

/// Borrow an owned environment for [`Client::start_env`].
pub fn refs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// Which hosts answer a ping: a fake `ping` for `ACS_PING` that succeeds for
/// the hosts listed in a file and records every host it is asked about, so
/// alias resolution (DESIGN §7.3) needs no ICMP.
pub struct Net {
    dir: TempDir,
}

impl Net {
    /// A ping answering for `up`.
    pub fn new(up: &[&str]) -> Net {
        let n = Net {
            dir: TempDir::new(),
        };
        let body = |ipv4_only: bool| {
            // macOS ping is IPv4-only: it exits 68 (EX_NOHOST) for an IPv6
            // address, and ping6 answers for those instead (acs-4pv).
            let refuse = match ipv4_only {
                true => "case \"$h\" in *:*) exit 68 ;; esac\n",
                false => "",
            };
            format!(
                "#!/bin/sh\nfor h; do :; done\nif grep -qx \"$h\" '{lag}'; then sleep 2; fi\necho \"$h\" >> '{log}'\n{refuse}if grep -qx \"$h\" '{slow}'; then sleep {slow_secs}; fi\ngrep -qx \"$h\" '{up}'\n",
                slow_secs = T.as_secs(),
                lag = n.lag_file().display(),
                log = n.pinged_file().display(),
                slow = n.slow_file().display(),
                up = n.up_file().display()
            )
        };
        for (path, ipv4_only) in [(n.ping(), true), (n.ping6(), false)] {
            write_executable(&path, body(ipv4_only));
        }
        n.set_up(up);
        n.set_slow(&[]);
        n.set_lagging(&[]);
        n
    }

    fn lag_file(&self) -> PathBuf {
        self.dir.path().join("lag")
    }

    /// Hosts whose ping takes 2 s to get as far as logging itself: what a
    /// machine too busy to schedule the fake's shell looks like, made to
    /// happen on every run rather than a few in sixty (acs-uj6).
    pub fn set_lagging(&self, lagging: &[&str]) {
        let mut s = lagging.join("\n");
        s.push('\n');
        std::fs::write(self.lag_file(), s).unwrap();
    }

    fn slow_file(&self) -> PathBuf {
        self.dir.path().join("slow")
    }

    /// Hosts whose ping takes [`T`] to say whether they answer: longer
    /// than a test waits for anything, shorter than [`Net::env`]'s
    /// `reachability_timeout`, so only a client that waits it out sees
    /// them answer.
    pub fn set_slow(&self, slow: &[&str]) {
        let mut s = slow.join("\n");
        s.push('\n');
        std::fs::write(self.slow_file(), s).unwrap();
    }

    fn ping(&self) -> PathBuf {
        self.dir.path().join("ping")
    }

    fn ping6(&self) -> PathBuf {
        self.dir.path().join("ping6")
    }

    fn up_file(&self) -> PathBuf {
        self.dir.path().join("up")
    }

    fn pinged_file(&self) -> PathBuf {
        self.dir.path().join("pinged")
    }

    pub fn set_up(&self, up: &[&str]) {
        let mut s = up.join("\n");
        s.push('\n');
        std::fs::write(self.up_file(), s).unwrap();
    }

    /// Every host pinged so far, sorted: an alias's hosts are pinged at
    /// once.
    ///
    /// **Sound only as a negative** ("X was never pinged"): the fake logs
    /// a host when its shell gets to run, which on a busy machine can be
    /// well after acs started it — and after acs has chosen another host
    /// and stopped waiting for this one. For "X was pinged" use
    /// [`Net::wait_pinged`] (acs-uj6).
    pub fn pinged(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_to_string(self.pinged_file())
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        v.sort();
        v
    }

    /// Wait until the hosts pinged are exactly `want`, in any order and a
    /// host twice for one pinged twice; after [`T`], fail with what was
    /// pinged instead. A ping acs started but did not wait for — a host
    /// that lost to the one chosen — logs itself whenever its shell is
    /// scheduled, so "X was pinged" has to wait for the line rather than
    /// sample the log once (acs-uj6).
    pub fn wait_pinged(&self, want: &[&str]) {
        let mut want: Vec<String> = want.iter().map(|h| h.to_string()).collect();
        want.sort();
        wait_until(
            T,
            || self.pinged() == want,
            || format!("hosts pinged: {:?}, want {want:?}", self.pinged()),
        );
    }

    /// The environment for a client with `config` as its configuration and
    /// this ping. This ping answers at once, but on a machine busy with
    /// every other test it may not even have *started* within the 500 ms
    /// default, so a configuration that sets no `reachability_timeout` gets
    /// one wider than the deadline the test itself waits to.
    ///
    /// Ten seconds was not enough (acs-ryz): under parallel copies of the
    /// suite with `dd` and fork loops beside them, `does not answer ping
    /// within 10s` was the commonest failure of the whole run — a shell
    /// script that had not been scheduled yet, reported as a host that is
    /// down. A ping that is going to answer answers in milliseconds, so
    /// nothing waits this out; what it buys is that [`T`] fires first, and
    /// a test that fails says which wait it was stuck in rather than
    /// blaming the network.
    pub fn env(&self, config: &str) -> Vec<(String, String)> {
        let config = if config.contains("reachability_timeout") {
            config.to_string()
        } else {
            format!("reachability_timeout: {}\n{config}", secs(OUTLAST_ONE_WAIT))
        };
        let mut env = config_env(self.dir.path(), &config);
        env.push(("ACS_PING".into(), self.ping().display().to_string()));
        env.push(("ACS_PING6".into(), self.ping6().display().to_string()));
        env
    }
}

/// A stand-in for the platform's network watcher (`netwatch.rs`, DESIGN
/// §5.3), in the two halves the client's decision is made of:
///
/// - the **hints** the kernel sends — a FIFO for `ACS_NETWATCH_FIFO`, where
///   the real client has a `PF_ROUTE` or `NETLINK_ROUTE` socket;
/// - the **networks** this machine is on — a file for `ACS_NETWATCH_NETS`,
///   where the real client has `getifaddrs`.
///
/// Both are needed to drive the decision rather than wait for a laptop to
/// roam (acs-6p8): a hint on its own says only that the kernel mentioned
/// the network, and whether that was a *change* is what the networks say.
pub struct NetWatch {
    dir: TempDir,
}

impl NetWatch {
    /// A watcher on a machine that is on `nets` (CIDR per line, as
    /// `getifaddrs` would report them).
    pub fn new(nets: &str) -> NetWatch {
        let w = NetWatch {
            dir: TempDir::new(),
        };
        make_fifo(&w.fifo());
        w.set_networks(nets);
        w
    }

    fn fifo(&self) -> PathBuf {
        self.dir.path().join("hints")
    }

    fn nets_file(&self) -> PathBuf {
        self.dir.path().join("networks")
    }

    /// The machine is on these networks from now on.
    pub fn set_networks(&self, nets: &str) {
        std::fs::write(self.nets_file(), nets).unwrap();
    }

    /// The kernel said something about the network: an address came or
    /// went, an interface changed state. Whether that is a *change* is for
    /// the client to work out from the networks.
    pub fn hint(&self) {
        release_fifo(&self.fifo());
    }

    /// The environment that points a client at this watcher.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "ACS_NETWATCH_FIFO".into(),
                self.fifo().display().to_string(),
            ),
            (
                "ACS_NETWATCH_NETS".into(),
                self.nets_file().display().to_string(),
            ),
        ]
    }
}

/// The shell a fake `ssh` runs first: one line per call, appended to `log`
/// under a lock.
///
/// The lock is not decoration. Several of these run at once — `acs list`
/// over every alias dials every host in parallel, and a redial overlaps the
/// master it is replacing — and a call's line is long, because it carries
/// the whole remote command: the prelude alone is 1046 bytes since acs-gov,
/// against 518 before it. `printf … >> log` goes through stdio, whose
/// buffer is 1024 bytes on macOS, so a line that size reaches the file in
/// two `write()`s and a second caller appends between them. The line then
/// arrives torn in half, and a test parsing it — `tests/list.rs` splits on
/// `" -- "` to read the destination — fails on a fragment that has no
/// destination in it. `mkdir` is atomic on every filesystem here, so it is
/// the lock.
pub fn log_call_sh(log: &Path) -> String {
    format!(
        "until mkdir '{lock}' 2>/dev/null; do sleep 0.01; done\n\
         printf '%s\\n' \"$*\" >> '{log}'\n\
         rmdir '{lock}'\n",
        log = log.display(),
        lock = log.with_extension("lock").display(),
    )
}

/// One recorded ssh call, split at the `--` that ends ssh's own options.
///
/// **Splitting is not optional, and getting it wrong is silent** (acs-9jv).
/// Everything after the `--` is the destination and then the remote
/// prelude, and the prelude is full of text shaped exactly like an ssh
/// option: `ls -ldnL`, `[ -L … ]`, `-i`. A test that asks whether a call
/// carried `-L` and searches the whole line finds one in a call that
/// carries no forward at all — and passes, asserting something untrue.
/// acs-odd hit precisely that while testing the `Call::Session` forwarding
/// gate, where the false positive would have hidden the thing under test.
///
/// This is the one splitter; the three that had grown up beside it
/// (`Ssh::keys`, an inline split in `tests/mux.rs`, `opts_of` in
/// `tests/config.rs`) and a fourth in `tests/list.rs` all go through it.
pub struct SshCall<'a> {
    /// Everything before the `--`: ssh's own options, and nothing else.
    pub opts: &'a str,
    /// The destination — the first word after the `--`.
    pub dest: &'a str,
    /// The remote command — the prelude — after the destination.
    pub remote: &'a str,
}

impl<'a> SshCall<'a> {
    /// Split one recorded call. Panics without a `--`: every call the
    /// client makes has one, so a line without one is a torn log line
    /// rather than a call to reason about (see [`log_call_sh`]).
    pub fn of(call: &'a str) -> SshCall<'a> {
        let (opts, rest) = call
            .split_once(" -- ")
            .unwrap_or_else(|| panic!("a destination after -- in {call:?}"));
        let (dest, remote) = rest.split_once(' ').unwrap_or((rest, ""));
        SshCall { opts, dest, remote }
    }

    /// The value of every `flag` (`-i`, `-L`, …) among the **options**, in
    /// order, and never one from the prelude. Both of ssh's spellings
    /// count: the value as its own word (`-L 1:h:2`) and attached to the
    /// flag (`-L1:h:2`), because a user's own options reach the command
    /// line verbatim.
    pub fn opt_values(&self, flag: &str) -> Vec<&'a str> {
        let opts: &'a str = self.opts;
        let words: Vec<&'a str> = opts.split(' ').collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < words.len() {
            if words[i] == flag {
                out.extend(words.get(i + 1).copied());
                i += 2;
            } else {
                match words[i].strip_prefix(flag) {
                    Some(v) if !v.is_empty() => out.push(v),
                    _ => {}
                }
                i += 1;
            }
        }
        out
    }
}

/// A fake `ssh` for `--ssh`: runs the remote command on the fake remote its
/// destination stands for, and refuses any other destination as a dead host
/// would. It records each call's arguments.
pub struct Ssh {
    dir: TempDir,
}

impl Ssh {
    pub fn new(hosts: &[(&str, &Remote)]) -> Ssh {
        let s = Ssh {
            dir: TempDir::new(),
        };
        let mut cases = String::new();
        for (dest, remote) in hosts {
            cases.push_str(&format!(
                "    '{dest}') exec '{}' \"$1\" ;;\n",
                remote.transport()
            ));
        }
        write_executable(
            &s.path(),
            format!(
                "#!/bin/sh\n{log}\
                 while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
                 dest=$2\nshift 2\n\
                 case \"$dest\" in\n{cases}esac\n\
                 echo \"ssh: connect to host $dest port 22: Connection refused\" >&2\nexit 255\n",
                log = log_call_sh(&s.log()),
            ),
        );
        s
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("ssh")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("calls")
    }

    /// The arguments of every call so far.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Every call so far as its destination and the keys (`-i`) it was
    /// given, in order.
    pub fn keys(&self) -> Vec<(String, Vec<String>)> {
        self.calls()
            .iter()
            .map(|c| {
                let call = SshCall::of(c);
                let keys = call
                    .opt_values("-i")
                    .into_iter()
                    .map(String::from)
                    .collect();
                (call.dest.to_string(), keys)
            })
            .collect()
    }
}

/// `cmd.output()`, retried while the program is "busy" (ETXTBSY): on Linux a
/// binary a test just copied cannot be run while another test thread's
/// fork still holds the copy's write descriptor.
pub fn output_of(cmd: &mut Command) -> std::process::Output {
    for _ in 0..50 {
        match cmd.output() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            r => return r.unwrap(),
        }
    }
    cmd.output().unwrap()
}

pub fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acs"))
}

/// Copy `src` to `dest` in a child `cp`, never in this process (acs-e86g).
///
/// A copy made here holds `dest` open for writing, and any other test
/// thread that forks in that window carries the write descriptor into its
/// child until the child's exec. Linux refuses to exec a file anything has
/// open for writing (ETXTBSY), so a remote's exec of a just-copied acs —
/// inside the fake ssh's `sh`, out of reach of [`output_of`]'s retry —
/// failed now and then under load. A `cp` child is the only process that
/// ever opens the file for writing, nothing forks from it, and it has
/// exited by the time this returns.
pub fn copy_out_of_process(src: &Path, dest: &Path) {
    let st = Command::new("cp")
        .arg(src)
        .arg(dest)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(
        st.success(),
        "cp {} {}: {st}",
        src.display(),
        dest.display()
    );
}

/// Write an executable `path` holding `body`, mode 0755, without this
/// process ever holding it open for writing — see [`copy_out_of_process`].
/// The body goes to a scratch file beside it first; that one may be "busy"
/// for a moment, but nothing runs it.
pub fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    let src = PathBuf::from(src);
    std::fs::write(&src, body).unwrap();
    copy_out_of_process(&src, path);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_file(&src);
}

/// Remove `path` (a file, or a directory and everything in it) when this
/// test binary exits, once no test is running. Only in the process that
/// asked: a forked child that calls `exit` inherits the handler, not the
/// job.
pub fn remove_at_exit(path: PathBuf) {
    static PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static OWNER: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    extern "C" fn remove_all() {
        if OWNER.get() != Some(&std::process::id()) {
            return;
        }
        let paths = PATHS.lock().map(|p| p.clone()).unwrap_or_default();
        for p in paths {
            let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
        }
    }
    OWNER.get_or_init(|| {
        // SAFETY: registers a plain function with no captured state.
        unsafe { libc::atexit(remove_all) };
        std::process::id()
    });
    PATHS.lock().unwrap().push(path);
}

/// `exe()` copied once per test binary onto `/tmp`, the filesystem every
/// [`TempDir`] lives on, so a remote's install can hard-link it even where
/// the build directory is on another filesystem (the Linux container's
/// target volume, where linking `exe()` itself is EXDEV). Removed when the
/// test binary exits.
pub fn staged_exe() -> &'static Path {
    static STAGED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    STAGED.get_or_init(|| {
        let p = PathBuf::from(format!("/tmp/acst-exe-{}", std::process::id()));
        copy_out_of_process(&exe(), &p);
        remove_at_exit(p.clone());
        p
    })
}

/// A copy of this acs installed the way Homebrew does under `dir`: the
/// binary in `homebrew/Cellar/acs/<version>/bin/`, linked from
/// `homebrew/bin/acs`, which is returned.
pub fn brewed_copy(dir: &Path) -> PathBuf {
    let keg = dir.join(format!("homebrew/Cellar/acs/{}/bin", acs::VERSION));
    std::fs::create_dir_all(&keg).unwrap();
    copy_out_of_process(&exe(), &keg.join("acs"));
    let bin = dir.join("homebrew/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join("acs");
    std::os::unix::fs::symlink(format!("../Cellar/acs/{}/bin/acs", acs::VERSION), &link).unwrap();
    link
}

/// A "remote host": its own HOME (with acs installed at the versioned path
/// the prelude looks for) and its own socket directory.
///
/// Its directory is never removed when it is dropped, only when the test
/// binary exits (acs-hx0m) — see the `Drop` below.
pub struct Remote {
    pub root: std::mem::ManuallyDrop<TempDir>,
}

/// Ends the remote's sessions now, as a [`TempDir`] would, but leaves its
/// directory to [`remove_at_exit`] (acs-hx0m).
///
/// The directory holds the remote's acs, a hard link of [`exe()`] (see
/// [`Remote::install`]). On macOS, unlinking such a link while another test
/// thread is spawning a client from [`exe()`] now and then gets that client
/// SIGKILLed 1–15 ms after the fork, before it writes a byte; nothing is
/// logged, and its test times out thirty seconds later. In `tests/client.rs`
/// with one test dropping two installed remotes at once, 10 runs in 270
/// failed so (several clients at a time); unlinking only the two binaries
/// and keeping the directories, 2 in 100; keeping everything, 0 in 160.
/// Removing the directories at exit, when no test is running and nothing is
/// spawning, takes the unlink out of that window: 0 in 150.
impl Drop for Remote {
    fn drop(&mut self) {
        acs::testutil::end_sessions_under(self.root.path());
        remove_at_exit(self.root.path().to_path_buf());
    }
}

impl Remote {
    pub fn new() -> Remote {
        let root = std::mem::ManuallyDrop::new(TempDir::new());
        let r = Remote { root };
        std::fs::create_dir_all(r.home()).unwrap();
        std::fs::create_dir_all(r.sockets()).unwrap();
        r
    }

    /// A remote with acs installed for the client's own version.
    pub fn installed() -> Remote {
        let r = Remote::new();
        r.install(acs::VERSION);
        r
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("h")
    }

    pub fn sockets(&self) -> PathBuf {
        self.root.path().join("s")
    }

    /// Install this acs as `version`: a real file, which is what a real host
    /// has — `acs _install` writes one — and what `current_exe()` on the
    /// remote side must name to behave like one.
    ///
    /// It was a *symlink* once, and that made the whole suite platform
    /// dependent (acs-cu8). Before exec'ing the remote binary the prelude
    /// reads its mode (acs-08m) and refuses anything group- or
    /// other-writable. A symlink's own mode is `lrwxrwxrwx` on Linux and
    /// `lrwxr-xr-x` on macOS, so a symlinked install is refused there and
    /// accepted here: in the container every one of these remotes looked
    /// *uninstalled*, paid a first-contact install, and every assertion
    /// counting connections or reading "not installed" said something
    /// different from what it says on a developer's laptop.
    pub fn install(&self, version: &str) {
        let dir = self.home().join(format!(".local/share/acs/{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let acs = dir.join("acs");
        // Not onto the file: it may be the acs a client of this remote is
        // still running, and Linux answers that with ETXTBSY.
        let _ = std::fs::remove_file(&acs);
        // A hard link where the temp directory and the build share a
        // filesystem, so that every remote's acs is the one inode the suite
        // already has warm — a hundred and thirty-odd private copies would
        // be a hundred and thirty cold execs of four megabytes. Where they
        // do not — the Linux container mounts the target directory as a
        // volume of its own, and `link` across that is EXDEV — a link to
        // the one staged copy on `/tmp` instead. Never a copy per remote
        // made in this process (acs-e86g): a test thread forking while it
        // was being written kept it "busy", and the remote's exec of it
        // failed ETXTBSY.
        if std::fs::hard_link(exe(), &acs).is_err()
            && std::fs::hard_link(staged_exe(), &acs).is_err()
        {
            copy_out_of_process(staged_exe(), &acs);
        }
    }

    /// File where the transport wrapper records the pid of every
    /// connection, so a test can cut one.
    pub fn pid_file(&self) -> PathBuf {
        self.root.path().join("transport.pids")
    }

    /// The `--transport-cmd` stand-in for ssh: a script that records its pid
    /// and runs the remote command in a shell with the remote's environment.
    ///
    /// **The body is written from this remote's current state on every
    /// call, not once** (acs-9jv). Every knob a test sets today lands in the
    /// file the script sources at run time, so the body never actually
    /// changes — which is exactly why a write-once script was a trap: the
    /// first setting that had to change the *body* would silently no-op
    /// whenever an earlier call had already materialised the file, and the
    /// symptom would be a test that configures something and sees no
    /// effect, with nothing anywhere saying why. Rewriting also matches how
    /// the sourced file already behaves: a setting made after an earlier
    /// `transport()` still reaches the next connection.
    ///
    /// The replacement is a rename, never a write in place: connections
    /// started earlier may still be running this script and `/bin/sh` reads
    /// it as it goes, so a truncate-and-write under them would hand a live
    /// shell a torn file. A rename leaves them the old inode and gives
    /// everyone after them the new one.
    pub fn transport(&self) -> String {
        let script = self.root.path().join("transport.sh");
        let body = format!(
            "#!/bin/sh\n\
             if [ -f '{refuse}' ]; then rm -f '{refuse}'; \
             echo 'ssh: connect to host devbox port 22: Connection refused' >&2; \
             exit 255; fi\n\
             echo $$ >> '{pids}'\n\
             [ -f '{delay}' ] && sleep \"$(cat '{delay}')\"\n\
             while [ -f '{gate}' ]; do sleep 0.05; done\n\
             if [ -f '{mute}' ]; then exec 3<&0; dd bs=1 count=1 of='{mute}'.$$ 2>/dev/null <&3; fi\n\
             [ -f '{silent}' ] && {{ cat '{silent}'; exec sleep {silent_secs}; }}\n\
             [ -f '{noise}' ] && cat '{noise}'\n\
             export HOME='{home}' ACS_SOCKET_DIR='{sock}' PATH='{fake}':\"$PATH\"\n\
             [ -f '{renv}' ] && . '{renv}'\n\
             if [ -f '{mute}' ]; then\n\
             mkfifo '{mute}'.$$.in\n\
             {{ cat '{mute}'.$$; cat <&3; }} > '{mute}'.$$.in &\n\
             exec /bin/sh -c \"$1\" < '{mute}'.$$.in\n\
             fi\n\
             exec /bin/sh -c \"$1\"\n",
            refuse = self.refuse_file().display(),
            pids = self.pid_file().display(),
            silent = self.silent_file().display(),
            silent_secs = OUTLAST_ONE_WAIT.as_secs(),
            delay = self.delay_file().display(),
            gate = self.gate_file().display(),
            mute = self.mute_file().display(),
            noise = self.noise_file().display(),
            home = self.home().display(),
            sock = self.sockets().display(),
            fake = self.fake_bin().display(),
            renv = self.remote_env_file().display(),
        );
        if std::fs::read_to_string(&script).ok().as_deref() != Some(body.as_str()) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = self.root.path().join(format!("transport.sh.{n}"));
            write_executable(&tmp, &body);
            std::fs::rename(&tmp, &script).unwrap();
        }
        script.display().to_string()
    }

    fn fake_bin(&self) -> PathBuf {
        self.root.path().join("fakebin")
    }

    fn noise_file(&self) -> PathBuf {
        self.root.path().join("login-noise")
    }

    fn silent_file(&self) -> PathBuf {
        self.root.path().join("silent")
    }

    fn delay_file(&self) -> PathBuf {
        self.root.path().join("delay")
    }

    fn gate_file(&self) -> PathBuf {
        self.root.path().join("dial-gate")
    }

    fn refuse_file(&self) -> PathBuf {
        self.root.path().join("dial-refused")
    }

    fn mute_file(&self) -> PathBuf {
        self.root.path().join("mute-until-greeted")
    }

    fn remote_env_file(&self) -> PathBuf {
        self.root.path().join("remote-env")
    }

    /// Add one shell line to the environment the transport sources before
    /// the remote command. **Every helper that configures the remote side
    /// goes through here, and here only ever appends** (acs-fzx): one that
    /// wrote the file whole would silently drop whatever the others had
    /// already set, and which setting survived would depend on the order a
    /// test happened to call them in — a test that still passes while no
    /// longer configuring what its name says it configures.
    fn remote_env_line(&self, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.remote_env_file())
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// Make the next connection ask for a password on the terminal the way
    /// ssh does — the terminal's echo off first, then the prompt, then a
    /// line read from `/dev/tty` — and return the file the answer is
    /// written to (acs-8ax). Like ssh's `readpassphrase`, it starts over
    /// when job control interrupts it: macOS does not restart a
    /// `tcsetattr` cut short by a stop.
    pub fn prompt_on_next_dial(&self) -> PathBuf {
        let flag = self.root.path().join("prompt-next");
        let answer = self.root.path().join("prompt-answer");
        self.remote_env_line(&format!(
            "if [ -f '{flag}' ]; then rm -f '{flag}'; until stty -echo </dev/tty 2>/dev/null; do :; done; \
             printf 'Password: ' >/dev/tty; read pw </dev/tty; stty echo </dev/tty; \
             echo \"$pw\" > '{answer}'; fi",
            flag = flag.display(),
            answer = answer.display(),
        ));
        std::fs::write(&flag, "").unwrap();
        answer
    }

    /// Run the remote side under `mask`, a restrictive umask (077) masking
    /// the mode files are created with.
    pub fn remote_umask(&self, mask: &str) {
        self.remote_env_line(&format!("umask {mask}"));
    }

    /// Turn the master's debug log on (`ACS_MASTER_LOG`). It is the only
    /// window a test has on what the master made of a link — whether it
    /// heard the client, and why it gave it up — and unlike a wall-clock
    /// wait it says so whatever the host's load is doing (acs-o6x). Call
    /// before the client starts; read with [`Remote::master_log`].
    pub fn log_master(&self) {
        self.remote_env_line(&format!(
            "export ACS_MASTER_LOG='{}'",
            self.master_log_file().display()
        ));
    }

    /// What the master has logged so far, empty until it writes its first
    /// line. See [`Remote::log_master`].
    pub fn master_log(&self) -> String {
        std::fs::read_to_string(self.master_log_file()).unwrap_or_default()
    }

    /// Where [`Remote::log_master`] points `ACS_MASTER_LOG`. Public so a
    /// test can assert the *exact* path the remote side was given (acs-9jv)
    /// rather than that it merely ends in `master.log` — a weak assertion
    /// like that passes with a path bug sitting under it.
    pub fn master_log_file(&self) -> PathBuf {
        self.root.path().join("master.log")
    }

    /// Environment for the remote side only (the proxy and the master),
    /// where the client's own must differ — different liveness timers, say.
    pub fn remote_env(&self, vars: &[(&str, &str)]) {
        for (k, v) in vars {
            self.remote_env_line(&format!("export {k}='{v}'"));
        }
    }

    /// Make new connections take `secs` seconds to reach the remote command,
    /// as a slow ssh handshake does; `None` makes them prompt again.
    pub fn slow_dial(&self, secs: Option<&str>) {
        match secs {
            Some(s) => std::fs::write(self.delay_file(), s).unwrap(),
            None => {
                let _ = std::fs::remove_file(self.delay_file());
            }
        }
    }

    /// Hold every new connection open just short of the remote command, as
    /// a dial still handshaking, until [`Remote::release_dial`] lets it
    /// through. Where [`Remote::slow_dial`] gives a test a stretch of wall
    /// clock it has to fit inside — and a host that stalls for longer than
    /// that turns a passing test red — this one ends when the test says so,
    /// so a stall only makes the test slower (acs-o8h).
    pub fn hold_dial(&self) {
        std::fs::write(self.gate_file(), "").unwrap();
    }

    /// Let the connection held by [`Remote::hold_dial`] finish.
    pub fn release_dial(&self) {
        let _ = std::fs::remove_file(self.gate_file());
    }

    /// Refuse the *next* connection out of hand, as a network still down
    /// does: nothing is spawned, so it is no connection at all
    /// ([`Remote::transport_pids`] does not count it) and the client is
    /// back in its backoff at once. The refusal is spent by that one
    /// connection; everything after it connects normally.
    ///
    /// **This is how a test puts the client in the offline wait**, now that
    /// the first redial after a drop goes at once (acs-iyq): cutting the
    /// link no longer leaves it there by itself, because that free attempt
    /// would succeed. Refusing it restores what a test used to get from the
    /// cut alone — the client waiting out `ACS_BACKOFF_MS` — with no wall
    /// clock involved, and it says so itself when it is there: only the
    /// second attempt onwards prints `reconnecting in <n>s`.
    pub fn refuse_one_dial(&self) {
        std::fs::write(self.refuse_file(), "").unwrap();
    }

    /// Make every connection say nothing at all — no login noise, no marker
    /// — until the client has written its first byte, and hand that byte on
    /// to the remote side unharmed (acs-trw). A client that waits for the
    /// marker before it greets deadlocks here, so it is the whole of the
    /// assertion that the HELLO goes out with the dial: no wall clock, and
    /// the remote's own order is the evidence.
    pub fn mute_until_greeted(&self) {
        std::fs::write(self.mute_file(), "").unwrap();
    }

    /// Make new connections go quiet once accepted, as a half-alive host
    /// does: they print `said` (nothing, or a marker) and then nothing more.
    /// `None` makes them normal again.
    pub fn silence(&self, said: Option<&str>) {
        match said {
            Some(s) => std::fs::write(self.silent_file(), s).unwrap(),
            None => {
                let _ = std::fs::remove_file(self.silent_file());
            }
        }
    }

    /// [`Remote::silence`] with bytes, for a host that says a frame or two
    /// after its marker and then nothing more.
    pub fn silence_bytes(&self, said: &[u8]) {
        std::fs::write(self.silent_file(), said).unwrap();
    }

    /// Make every connection print `text` on stdout before the remote
    /// command runs, as a chatty `.bashrc` or motd script would.
    pub fn login_noise(&self, text: &str) {
        self.login_noise_bytes(text.as_bytes());
    }

    /// [`Remote::login_noise`] with bytes, for a banner that is not UTF-8
    /// (a Latin-1 or CP437 motd).
    pub fn login_noise_bytes(&self, text: &[u8]) {
        std::fs::write(self.noise_file(), text).unwrap();
    }

    /// Make the remote's `uname -s` / `uname -m` report another platform.
    pub fn fake_uname(&self, os: &str, arch: &str) {
        std::fs::create_dir_all(self.fake_bin()).unwrap();
        let script = self.fake_bin().join("uname");
        write_executable(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo {os};; -m) echo {arch};; *) echo {os};; esac\n"
            ),
        );
    }

    /// The installed binary for `version`, if any.
    pub fn installed_binary(&self, version: &str) -> PathBuf {
        self.home().join(format!(".local/share/acs/{version}/acs"))
    }

    /// Pids of transport connections made so far.
    pub fn transport_pids(&self) -> Vec<i32> {
        std::fs::read_to_string(self.pid_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// Cut the most recent connection, as a network drop would.
    pub fn cut_link(&self) {
        let pid = *self.transport_pids().last().expect("no connection to cut");
        let _ = sys::kill(pid, libc::SIGKILL);
    }

    /// How many connections have been made so far: the baseline a test
    /// takes before the stimulus it is about, and counts further
    /// connections from with [`Remote::wait_more_connections`].
    ///
    /// **There is no absolute-count wait any more** (acs-ryz). One existed,
    /// and it invited the bug acs-cu8 spent a morning on: how many
    /// connections a client has made by the time a test is ready to disturb
    /// it is not a constant. A remote that has to install acs first spends
    /// one or two before the session — which is what the Linux container
    /// does and a developer's laptop does not — so `wait_connections(2)`
    /// was already satisfied before the cut it was meant to observe, and
    /// the test passed without testing anything. Five call sites had grown
    /// a hand-written baseline against it; this makes the baseline the only
    /// way to ask.
    pub fn connections(&self) -> usize {
        self.transport_pids().len()
    }

    /// Wait until `extra` more connections have been made than the `base`
    /// [`Remote::connections`] returned before the stimulus.
    pub fn wait_more_connections(&self, base: usize, extra: usize, timeout: Duration) {
        wait_until(
            timeout,
            || self.connections() >= base + extra,
            || {
                format!(
                    "{} connections, waiting for {extra} more than the {base} there were",
                    self.connections(),
                )
            },
        );
    }

    /// Run the remote command a real ssh would run (the prelude), with the
    /// remote's environment, over pipes.
    pub fn run_remote(&self, args: &[&str]) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(acs::ssh::remote_acs(acs::VERSION, args))
            .env("HOME", self.home())
            .env("ACS_SOCKET_DIR", self.sockets())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }

    pub fn session_exists(&self, name: &str) -> bool {
        std::os::unix::net::UnixStream::connect(self.sockets().join(format!("{name}.sock"))).is_ok()
    }
}

/// The acs client running on a pty, as a user would run it.
pub struct Client {
    pub child: Child,
    pty: OwnedFd,
    output: Arc<Mutex<Vec<u8>>>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    searched: usize,
}

impl Client {
    /// `acs --transport-cmd <remote transport> <args...>` on an 80x24 pty.
    pub fn start(remote: &Remote, args: &[&str]) -> Client {
        Client::start_env(remote, args, &[])
    }

    pub fn start_env(remote: &Remote, args: &[&str], env: &[(&str, &str)]) -> Client {
        Client::start_exe(&exe(), remote, args, env)
    }

    /// Run a specific acs binary as the client.
    pub fn start_exe(exe: &Path, remote: &Remote, args: &[&str], env: &[(&str, &str)]) -> Client {
        let mut full = vec!["--transport-cmd".to_string(), remote.transport()];
        full.extend(args.iter().map(|s| s.to_string()));
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        Client::spawn(exe, &full, env)
    }

    /// Run `exe` with exactly `args` on a pty (real ssh, no fake remote).
    pub fn spawn(exe: &Path, args: &[&str], env: &[(&str, &str)]) -> Client {
        let (master, slave) = sys::openpty().unwrap();
        sys::set_winsize(
            slave.as_raw_fd(),
            &acs::proto::WinSize {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
        )
        .unwrap();
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd.env("TERM", "xterm-256color")
            .env_remove("ACS_DEFAULT_SESSION")
            .env_remove("ACS_SOCKET_DIR")
            // Never the developer's own configuration files, and never
            // GitHub.
            .env("ACS_NO_UPDATE_CHECK", "1")
            .env("XDG_CONFIG_HOME", NO_CONFIG)
            .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
            .env("XDG_STATE_HOME", NO_STATE)
            .env("ACS_IDENTITY", "tester@local")
            // And never a shared ssh master in the developer's own
            // /tmp/acs-mux-<uid> (acs-9n3); tests/mux.rs sets its own.
            .env("ACS_CONTROL_PERSIST", NO_MASTER)
            // And never the developer's own network. A path that does not
            // exist leaves the client with no watcher at all, so a Wi-Fi
            // roam or a VPN coming up on the machine running the suite
            // cannot cut an offline wait short under a test that is not
            // about the watcher (acs-6p8). The tests that *are* about it
            // pass their own `ACS_NETWATCH_FIFO` in `env` below, which is
            // applied after this one and wins.
            .env("ACS_NETWATCH_FIFO", "/nonexistent/acs-test-netwatch");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let s = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        cmd.stdin(s(&slave)).stdout(s(&slave)).stderr(s(&slave));
        // SAFETY: async-signal-safe calls only.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(slave);
        let output = Arc::new(Mutex::new(Vec::new()));
        let out = output.clone();
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = paused.clone();
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                while p.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Client {
            child,
            pty: master,
            output,
            paused,
            searched: 0,
        }
    }

    /// Stop (or start again) draining the terminal, as an emulator that
    /// stalled does: the client's writes to stdout then block.
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The client's own process id, for a test that has to take the CPU
    /// away from it (`SIGSTOP`) at a moment of its choosing rather than
    /// hope a loaded machine does it — which is how acs-br2 was reproduced.
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Type bytes.
    pub fn send(&self, bytes: &[u8]) {
        sys::write_all(self.pty.as_raw_fd(), bytes).unwrap();
    }

    /// Wait until `ready` holds, or fail at the deadline. For the moments
    /// where a test needs the client to have *reached* a state rather than
    /// to have been given time to (acs-kip). The free [`wait_until`], with
    /// the client's output in its failure.
    #[track_caller]
    pub fn wait_until(&self, what: &str, mut ready: impl FnMut(&Self) -> bool, timeout: Duration) {
        wait_until(
            timeout,
            || ready(self),
            || format!("waiting until {what}; output so far:\n{}", self.text()),
        );
    }

    /// How many bytes the client wrote to its terminal so far, without
    /// copying them: for output too large to search (tests/throughput.rs).
    pub fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Everything the client wrote to its terminal so far.
    pub fn output(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output()).into_owned()
    }

    /// Wait until the output (after anything already waited for) contains
    /// `needle`.
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let n = needle.as_bytes();
        loop {
            {
                let out = self.output.lock().unwrap();
                // From the end of the last match: backing up would find
                // that match again.
                let start = self.searched;
                if let Some(p) = out[start..].windows(n.len()).position(|w| w == n) {
                    self.searched = start + p + n.len();
                    return;
                }
            }
            if Instant::now() >= deadline {
                // Whether the client is still there: a client that died
                // silently (acs-hx0m) reads very differently from one that
                // is merely waiting.
                let st = self.child.try_wait();
                panic!(
                    "timed out waiting for {needle:?}; client {}: {st:?}; output so far:\n{}",
                    self.child.id(),
                    self.text()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until a redial has resumed: the status line shown while the link
    /// was down is taken away on WELCOME (its title popped). Keys typed
    /// before then are dropped (DESIGN §5.2), so a test types after it.
    pub fn wait_resumed(&mut self) {
        self.wait_for("\x1b[23;0t", T);
    }

    /// Wait until the client announces the session it created — `new
    /// session '<name>' on <host>`, printed on the WELCOME that created it.
    ///
    /// **This is the anchor a short needle from the program needs** under
    /// `-v` (acs-ryz). Verbose echoes the whole remote prelude into the
    /// terminal before the dial is answered — a kilobyte of shell script —
    /// and [`Client::wait_for`] searches forward from the end of the last
    /// match, so a wait for the two letters `up` is satisfied by any word
    /// of that script containing them, before the program has run at all.
    /// It has happened: a refusal that named the *group* write bit passed
    /// several tests that were waiting for `echo up`, and `src/ssh.rs`
    /// still carries the reworded message. Waiting for this line first puts
    /// the echo behind the cursor, so what is found afterwards is the
    /// session's own output and can be nothing else.
    pub fn wait_session(&mut self, name: &str) {
        self.wait_for(&format!("new session '{name}' on "), T);
    }

    /// Change the terminal size (the client gets SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) {
        sys::set_winsize(
            self.pty.as_raw_fd(),
            &acs::proto::WinSize {
                cols,
                rows,
                xpixel: 0,
                ypixel: 0,
            },
        )
        .unwrap();
    }

    /// Terminal settings of the client's pty (raw or cooked).
    pub fn echo_on(&self) -> bool {
        let t = sys::tcgetattr(self.pty.as_raw_fd()).unwrap();
        t.c_lflag & libc::ECHO != 0
    }

    /// Wait for the client to exit; returns its exit code.
    #[track_caller]
    pub fn wait(&mut self, timeout: Duration) -> i32 {
        use std::os::unix::process::ExitStatusExt;
        let (child, output) = (&mut self.child, &self.output);
        let st = poll(
            timeout,
            || child.try_wait().unwrap(),
            || {
                format!(
                    "client did not exit; output:\n{}",
                    String::from_utf8_lossy(&output.lock().unwrap())
                )
            },
        );
        st.code().unwrap_or(128 + st.signal().unwrap_or(0))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The client's terminal with everything the *client* wrote taken out, so
/// that what is left is the program's own stream and a `#175#` the client
/// wrote through still reads as one number.
///
/// Two things land in the middle of it, and neither loses a byte:
///
/// - **the status line**, whose save/restore-cursor span (`ESC 7` … `ESC 8`)
///   is written wherever the cursor happens to be;
/// - **an `acs: …` note**, which goes to fd 2 while the session's bytes go
///   to fd 1, so on one terminal it arrives wherever the stream has got to.
///   acs-z22 holds a note until the program's stream is at a boundary, and
///   that ended the corruption it was raised for — but every byte position
///   in a plain-ASCII `#175#` *is* a boundary, so a note still comes out
///   between the `#17` and the `5#` (acs-2dc).
///
/// A note is recognised the way the client decides where one may go: at a
/// **boundary** of the stream, by the same [`acs::modes::ModeObserver`].
/// `acs: ` inside an escape sequence is not a note but somebody's text —
/// the status line's own window title says `ESC ] 2 ; acs: <host> — …`,
/// and reading that as a note cut the line, and the next number with it.
///
/// A span still being written when the snapshot was taken ends the text:
/// what follows it has not arrived yet.
pub fn program_output(text: &str) -> String {
    let b = text.as_bytes();
    let mut kept: Vec<u8> = Vec::with_capacity(b.len());
    // What "a note" means is what it means to the client: text at a
    // boundary of the stream, not five characters that happen to read
    // `acs: `. The status line writes its own message into an OSC title
    // (`ESC ] 2 ; acs: <host> — …`, src/reconnect.rs) where those five
    // characters are the title's body, and cutting the line from there
    // took the next number with it.
    let mut stream = acs::modes::ModeObserver::new();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"\x1b7") {
            match b[i..].windows(2).position(|w| w == b"\x1b8") {
                Some(j) => i += j + 2,
                None => return String::from_utf8_lossy(&kept).into_owned(),
            }
            continue;
        }
        if stream.at_boundary() && b[i..].starts_with(b"acs: ") {
            match b[i..].iter().position(|&c| c == b'\n') {
                Some(j) => i += j + 1,
                None => return String::from_utf8_lossy(&kept).into_owned(),
            }
            continue;
        }
        stream.observe(&b[i..i + 1]);
        kept.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// The `#n#` numbers a ticker printed, in order.
///
/// **A number counts only once both its `#`s are there.** The text is a
/// snapshot of a stream still being written, so its last bytes are as
/// likely as not to be the first half of one — and `#17` read as 17 where
/// `#175#` was being written is how acs-ryz's one unexplained failure
/// reported itself, as `lost or repeated output around 174 → 17`. A
/// trailing fragment is not output that was lost; it is output that has
/// not arrived yet, and reading it as a number turns the next wait into a
/// hunt for a `#37#` the program printed long ago.
pub fn numbers(text: &str) -> Vec<u64> {
    let clean = program_output(text);
    let mut parts: Vec<&str> = clean.split('#').collect();
    // Whatever follows the last `#` has no `#` of its own yet.
    parts.pop();
    parts.iter().filter_map(|p| p.parse().ok()).collect()
}

/// Every number the ticker printed is there, once, in order.
pub fn assert_consecutive(text: &str) {
    let n = numbers(text);
    assert!(n.len() > 10, "too little output: {n:?}");
    for w in n.windows(2) {
        assert!(
            w[1] == w[0] + 1,
            "lost or repeated output around {} → {}\n{}",
            w[0],
            w[1],
            around(text, w[0]),
        );
    }
}

/// The raw stream either side of `#n#`, escaped — what a failure needs to
/// say **which** of two different things happened (acs-2dc):
///
/// - output **torn**: the bytes are all there, with something the client
///   wrote sitting in the middle of a number;
/// - output **lost**: the stream steps from one number to another with
///   nothing between them, which is the protocol failure this suite is
///   actually guarding.
///
/// From the numbers alone the two are indistinguishable, and acs-ryz spent
/// its one unexplained burst on the wrong one.
fn around(text: &str, n: u64) -> String {
    let needle = format!("#{n}#");
    let Some(i) = text.rfind(&needle) else {
        return format!("({needle:?} is not in the stream at all)");
    };
    let from = text[..i]
        .char_indices()
        .rev()
        .take(160)
        .last()
        .map_or(0, |(j, _)| j);
    let to = text[i..]
        .char_indices()
        .take(160)
        .last()
        .map_or(text.len(), |(j, c)| i + j + c.len_utf8());
    format!("the stream around {needle}: {:?}", &text[from..to])
}

/// The last whole number the client has printed.
pub fn last_number(c: &Client) -> u64 {
    *numbers(&c.text()).last().unwrap_or(&0)
}

/// Ctrl-] Ctrl-] then `key`.
pub fn command(key: u8) -> Vec<u8> {
    vec![0x1d, 0x1d, key]
}

/// Fast client timers, so a resume takes a fraction of a second.
pub const FAST: &[(&str, &str)] = &[
    ("ACS_BACKOFF_MS", "100"),
    ("ACS_PING_MS", "200"),
    ("ACS_DEAD_MS", "800"),
];

/// A program printing `#1#`, `#2#`, … a line every 10 ms, for
/// [`numbers`] and [`assert_consecutive`] to read.
pub const TICKER: &str = "i=0; while true; do i=$((i+1)); printf '#%d#\\n' $i; sleep 0.01; done";

/// A program that reports every byte it receives as `in:xx`, as it comes;
/// [`received`] reads the reports back.
pub const REPORTER: &str = "stty raw -echo; echo ready; \
    while :; do b=$(dd bs=1 count=1 2>/dev/null | od -An -tx1); printf 'in:%s\\n' $b; done";

/// The bytes a [`REPORTER`] reported, in order, as two hex digits each.
pub fn received(c: &Client) -> Vec<String> {
    c.text()
        .split("in:")
        .skip(1)
        .filter_map(|s| s.get(..2).map(String::from))
        .collect()
}

/// The command of a session that says `up` and stays: `-- /bin/sh -c …`.
pub const UP_CMD: &[&str] = &["--", "/bin/sh", "-c", "echo up; sleep 30"];

/// The arguments for session `session` on `devbox` running [`UP_CMD`].
pub fn up_args(session: &str) -> Vec<&str> {
    let mut a = vec!["devbox", session];
    a.extend_from_slice(UP_CMD);
    a
}

/// A session that answers each line typed with `got-<name>:<line>`, after
/// saying `ready-<name>`.
pub fn echo(name: &str) -> String {
    format!("echo ready-{name}; while read l; do echo got-{name}:$l; done")
}

/// The last screen the session menu drew in full: a redraw may be read
/// half-way, so one that has not reached its closing erase yet is skipped.
pub fn last_screen(c: &Client) -> String {
    let text = c.text();
    let mut screens: Vec<&str> = text.split("\x1b[Hacs: ").skip(1).collect();
    if screens.last().is_some_and(|s| !s.contains("\x1b[J")) && screens.len() > 1 {
        screens.pop();
    }
    let last = screens.last().expect("no menu drawn");
    format!("\x1b[Hacs: {last}")
}

/// The number of session `name`'s row in the [`last_screen`] of the menu.
pub fn menu_row(c: &Client, name: &str) -> usize {
    let screen = last_screen(c);
    let line = screen
        .split("\r\n")
        .find(|l| l.contains(&format!(" {name} ")))
        .unwrap_or_else(|| panic!("no {name} in {screen:?}"));
    let line = line.trim_start_matches("\x1b[7m");
    line[2..3].parse().unwrap()
}

/// A data file with every byte value except `\n` (a pty turns `\n` into
/// `\r\n`), so output can be compared byte for byte.
pub fn binary_pattern(path: &Path, len: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..len)
        .map(|i| {
            let b = (i * 7 + i / 251) as u8;
            if b == b'\n' {
                b'N'
            } else {
                b
            }
        })
        .collect();
    std::fs::write(path, &data).unwrap();
    data
}

/// gzip -9 -n of `data`.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new("gzip")
        .args(["-9", "-n", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let w = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().unwrap();
    w.join().unwrap().unwrap();
    out.stdout
}

/// A "complete" test client: this acs binary with a payload set appended
/// whose `target` entry is this same binary (so a fake remote of that
/// platform can run what gets installed).
pub fn complete_client(dir: &Path, target: &str) -> (PathBuf, Vec<u8>, Vec<u8>) {
    let (slim, gz) = slim_and_gz();
    let blob = acs::payload::build(&[acs::payload::Input {
        target,
        raw: slim,
        gz,
    }]);
    let path = dir.join("acs-complete");
    let mut full = slim.clone();
    full.extend_from_slice(&blob);
    std::fs::write(&path, &full).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, slim.clone(), blob)
}

/// This acs binary and its `gzip -9`, read and compressed once per test
/// binary (acs-dz3r): the binary does not change while its tests run, and a
/// `gzip -9` of its ~5 MB costs about 2 s of CPU per call.
fn slim_and_gz() -> (&'static Vec<u8>, &'static Vec<u8>) {
    static PAIR: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
    let (slim, gz) = PAIR.get_or_init(|| {
        let slim = std::fs::read(exe()).unwrap();
        let gz = gzip(&slim);
        (slim, gz)
    });
    (slim, gz)
}

/// What a missing `ssh-keygen` is reported as. `Command::status()` answers
/// one with a bare `No such file or directory` that does not say which file
/// — on a stripped image that reading cost an afternoon (acs-cu8), so the
/// tool is named here.
const NEEDS_KEYGEN: &str =
    "ssh-keygen must be on PATH: the release signature is made and checked with it (acs-o9v)";

/// The release signing pair, `(private key file, public key line)`, made
/// once per test binary (acs-dz3r) rather than by an `ssh-keygen` per
/// server: every server signs with it, and the one test that needs a
/// release signed by somebody else makes that key itself
/// (`sign_with_another_key`).
fn release_pair() -> &'static (String, String) {
    static PAIR: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    PAIR.get_or_init(|| {
        let dir = TempDir::new();
        let key = dir.path().join("signer");
        let st = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&key)
            .status()
            .expect(NEEDS_KEYGEN);
        assert!(st.success());
        let private = std::fs::read_to_string(&key).unwrap();
        let public = std::fs::read_to_string(dir.path().join("signer.pub")).unwrap();
        (private, public.trim().to_string())
    })
}

/// A stand-in for GitHub Releases over plain HTTP, for `ACS_RELEASES_URL`:
/// the real `curl` downloads from it. Paths are as on GitHub
/// (`/latest/download/…`, `/download/v<version>/…`).
pub struct ReleaseServer {
    pub url: String,
    files: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    hits: Arc<Mutex<Vec<String>>>,
    root: TempDir,
}

impl ReleaseServer {
    pub fn start() -> ReleaseServer {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let files: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let (f, h) = (files.clone(), hits.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let path = text
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_string();
                h.lock().unwrap().push(path.clone());
                let body = f.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => conn
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                b.len()
                            )
                            .as_bytes(),
                        )
                        .and_then(|_| conn.write_all(&b)),
                    None => conn.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ),
                };
            }
        });
        ReleaseServer {
            url,
            files,
            hits,
            root: TempDir::new(),
        }
    }

    pub fn put(&self, path: &str, data: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), data.into());
    }

    /// Stop serving `path`, as a release published without that file would.
    pub fn remove(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }

    /// The public half of the key this server signs its releases with
    /// (acs-o9v), for `ACS_RELEASE_KEY`.
    pub fn release_key(&self) -> String {
        release_pair().1.clone()
    }

    /// The private half's path, written into this server's directory on
    /// first use.
    fn signing_key(&self) -> PathBuf {
        let key = self.root.path().join("signer");
        if !key.is_file() {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&key)
                .unwrap();
            f.write_all(release_pair().0.as_bytes()).unwrap();
        }
        key
    }

    /// Sign `sums` as a release does and serve the signature at
    /// `<path>.sig`; `key` overrides the server's own, for the test that a
    /// release signed by somebody else is refused.
    fn publish_signature(&self, path: &str, sums: &str, key: Option<&Path>) {
        let owned = self.signing_key();
        let key = key.unwrap_or(&owned);
        let file = self.root.path().join("to-sign");
        std::fs::write(&file, sums).unwrap();
        let st = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", acs::signature::NAMESPACE, "-f"])
            .arg(key)
            .arg(&file)
            .status()
            .expect(NEEDS_KEYGEN);
        assert!(st.success());
        let sig = std::fs::read(self.root.path().join("to-sign.sig")).unwrap();
        std::fs::remove_file(self.root.path().join("to-sign.sig")).unwrap();
        self.put(&format!("{path}.sig"), sig);
    }

    /// Re-sign every `SHA256SUMS` served with a key that is not this
    /// server's, as a release published by someone who took the account
    /// over would be (acs-o9v).
    pub fn sign_with_another_key(&self) {
        let other = self.root.path().join("impostor");
        if !other.is_file() {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "them", "-f"])
                .arg(&other)
                .status()
                .expect(NEEDS_KEYGEN);
            assert!(st.success());
        }
        let paths: Vec<String> = (self.files.lock().unwrap().keys())
            .filter(|p| p.ends_with("SHA256SUMS"))
            .cloned()
            .collect();
        for p in paths {
            let sums = self.files.lock().unwrap().get(&p).cloned().unwrap();
            let sums = String::from_utf8(sums).unwrap();
            self.publish_signature(&p, &sums, Some(&other));
        }
    }

    /// Paths requested so far.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    /// A fake acs for `version`: a script that answers `--version` as acs
    /// does.
    pub fn fake_acs(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\necho 'acs {version} (protocol 9, {t})'\necho 'installs remotes: {t}'\n",
            t = acs::payload::OWN_TARGET
        )
        .into_bytes()
    }

    /// Publish `binary` as release `version` for this machine's target, with
    /// a matching SHA256SUMS (or a wrong one, with `tamper`). `latest` also
    /// makes it the latest release.
    pub fn release(&self, version: &str, binary: &[u8], latest: bool, tamper: bool) {
        let target = acs::release::archive_target(acs::payload::OWN_TARGET);
        let name = format!("acs-{version}-{target}");
        let stage = self.root.path().join(version);
        std::fs::create_dir_all(stage.join(&name)).unwrap();
        let bin = stage.join(&name).join("acs");
        std::fs::write(&bin, binary).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = stage.join(format!("{name}.tar.gz"));
        let st = Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg(&name)
            .status()
            .unwrap();
        assert!(st.success());
        let data = std::fs::read(&archive).unwrap();
        let mut sum = acs::sha256::hex(&acs::sha256::digest(&data));
        if tamper {
            sum = "0".repeat(64);
        }
        // Another target's line first, as in a real SHA256SUMS.
        let sums = format!(
            "{}  acs-{version}-riscv64-unknown-linux-musl.tar.gz\n{sum}  {name}.tar.gz\n",
            "a".repeat(64)
        );
        // Tagged as the real releases are: `v` up to 0.18.0, bare after.
        let tag = acs::release::release_tag(version);
        self.put(&format!("/download/{tag}/{name}.tar.gz"), data);
        let versioned = format!("/download/{tag}/SHA256SUMS");
        self.put(&versioned, sums.clone());
        self.publish_signature(&versioned, &sums, None);
        if latest {
            self.put("/latest/download/SHA256SUMS", sums.clone());
            self.publish_signature("/latest/download/SHA256SUMS", &sums, None);
        }
    }
}
