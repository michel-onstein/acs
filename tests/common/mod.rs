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

/// The deadline test helpers wait to. Generous on purpose (acs-kip): the
/// whole suite runs its targets in parallel, so a laptop building and
/// running two dozen binaries at once takes many times longer over any one
/// step than an idle machine does. A test that truly hangs still fails,
/// only later; one that merely lost the CPU for a second does not, and a
/// gate that cries wolf teaches people to re-run rather than to read.
pub const T: Duration = Duration::from_secs(30);

/// A directory that does not exist: the client finds no configuration file
/// unless a test gives it one.
pub const NO_CONFIG: &str = "/nonexistent/acs-test-config";

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
                "#!/bin/sh\nfor h; do :; done\necho \"$h\" >> '{log}'\n{refuse}if grep -qx \"$h\" '{slow}'; then sleep 30; fi\ngrep -qx \"$h\" '{up}'\n",
                log = n.pinged_file().display(),
                slow = n.slow_file().display(),
                up = n.up_file().display()
            )
        };
        use std::os::unix::fs::PermissionsExt;
        for (path, ipv4_only) in [(n.ping(), true), (n.ping6(), false)] {
            std::fs::write(&path, body(ipv4_only)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        n.set_up(up);
        n.set_slow(&[]);
        n
    }

    fn slow_file(&self) -> PathBuf {
        self.dir.path().join("slow")
    }

    /// Hosts whose ping takes 30 s to say whether they answer.
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
    pub fn pinged(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_to_string(self.pinged_file())
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        v.sort();
        v
    }

    /// The environment for a client with `config` as its configuration and
    /// this ping. This ping answers at once, but on a machine busy with every
    /// other test it may not even have started within the 500 ms default, so
    /// a configuration that sets no `reachability_timeout` gets 10 s.
    pub fn env(&self, config: &str) -> Vec<(String, String)> {
        let config = if config.contains("reachability_timeout") {
            config.to_string()
        } else {
            format!("reachability_timeout: 10s\n{config}")
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
        let path = std::ffi::CString::new(w.fifo().to_str().unwrap()).unwrap();
        // SAFETY: a path in this test's own temporary directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
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
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(self.fifo())
            .unwrap();
        f.write_all(b"x").unwrap();
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
        std::fs::write(
            s.path(),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
                 dest=$2\nshift 2\n\
                 case \"$dest\" in\n{cases}esac\n\
                 echo \"ssh: connect to host $dest port 22: Connection refused\" >&2\nexit 255\n",
                log = s.log().display(),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
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
                let (opts, rest) = c.split_once(" -- ").expect("a destination after --");
                let opts: Vec<&str> = opts.split(' ').collect();
                let keys = opts
                    .windows(2)
                    .filter(|w| w[0] == "-i")
                    .map(|w| w[1].to_string())
                    .collect();
                let dest = rest.split(' ').next().unwrap().to_string();
                (dest, keys)
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

/// A copy of this acs installed the way Homebrew does under `dir`: the
/// binary in `homebrew/Cellar/acs/<version>/bin/`, linked from
/// `homebrew/bin/acs`, which is returned.
pub fn brewed_copy(dir: &Path) -> PathBuf {
    let keg = dir.join(format!("homebrew/Cellar/acs/{}/bin", acs::VERSION));
    std::fs::create_dir_all(&keg).unwrap();
    std::fs::copy(exe(), keg.join("acs")).unwrap();
    let bin = dir.join("homebrew/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join("acs");
    std::os::unix::fs::symlink(format!("../Cellar/acs/{}/bin/acs", acs::VERSION), &link).unwrap();
    link
}

/// A "remote host": its own HOME (with acs installed at the versioned path
/// the prelude looks for) and its own socket directory.
pub struct Remote {
    pub root: TempDir,
}

impl Remote {
    pub fn new() -> Remote {
        let root = TempDir::new();
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
        // be a hundred and thirty cold execs of four megabytes. A copy
        // where they do not: the Linux container mounts the target
        // directory as a volume of its own, and `link` across that is
        // EXDEV.
        if std::fs::hard_link(exe(), &acs).is_err() {
            std::fs::copy(exe(), &acs).unwrap();
        }
    }

    /// File where the transport wrapper records the pid of every
    /// connection, so a test can cut one.
    pub fn pid_file(&self) -> PathBuf {
        self.root.path().join("transport.pids")
    }

    /// The `--transport-cmd` stand-in for ssh: a script that records its pid
    /// and runs the remote command in a shell with the remote's environment.
    pub fn transport(&self) -> String {
        let script = self.root.path().join("transport.sh");
        if !script.exists() {
            let body = format!(
                "#!/bin/sh\n\
                 if [ -f '{refuse}' ]; then rm -f '{refuse}'; \
                 echo 'ssh: connect to host devbox port 22: Connection refused' >&2; \
                 exit 255; fi\n\
                 echo $$ >> '{pids}'\n\
                 [ -f '{delay}' ] && sleep \"$(cat '{delay}')\"\n\
                 while [ -f '{gate}' ]; do sleep 0.05; done\n\
                 if [ -f '{mute}' ]; then exec 3<&0; dd bs=1 count=1 of='{mute}'.$$ 2>/dev/null <&3; fi\n\
                 [ -f '{silent}' ] && {{ cat '{silent}'; exec sleep 60; }}\n\
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
                delay = self.delay_file().display(),
                gate = self.gate_file().display(),
                mute = self.mute_file().display(),
                noise = self.noise_file().display(),
                home = self.home().display(),
                sock = self.sockets().display(),
                fake = self.fake_bin().display(),
                renv = self.remote_env_file().display(),
            );
            std::fs::write(&script, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    fn master_log_file(&self) -> PathBuf {
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
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo {os};; -m) echo {arch};; *) echo {os};; esac\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    /// Wait until `n` connections have been made.
    pub fn wait_connections(&self, n: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.transport_pids().len() < n {
            assert!(
                Instant::now() < deadline,
                "only {} connections",
                self.transport_pids().len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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

    /// Type bytes.
    pub fn send(&self, bytes: &[u8]) {
        sys::write_all(self.pty.as_raw_fd(), bytes).unwrap();
    }

    /// Wait until `ready` holds, or fail at the deadline. For the moments
    /// where a test needs the client to have *reached* a state rather than
    /// to have been given time to (acs-kip).
    pub fn wait_until(&self, what: &str, mut ready: impl FnMut(&Self) -> bool, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !ready(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting until {what}; output so far:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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
                panic!(
                    "timed out waiting for {needle:?}; output so far:\n{}",
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
    pub fn wait(&mut self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                use std::os::unix::process::ExitStatusExt;
                return st.code().unwrap_or(128 + st.signal().unwrap_or(0));
            }
            if Instant::now() >= deadline {
                panic!("client did not exit; output:\n{}", self.text());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ctrl-] Ctrl-] then `key`.
pub fn command(key: u8) -> Vec<u8> {
    vec![0x1d, 0x1d, key]
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
    let slim = std::fs::read(exe()).unwrap();
    let gz = gzip(&slim);
    let blob = acs::payload::build(&[acs::payload::Input {
        target,
        raw: &slim,
        gz: &gz,
    }]);
    let path = dir.join("acs-complete");
    let mut full = slim.clone();
    full.extend_from_slice(&blob);
    std::fs::write(&path, &full).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, slim, blob)
}

/// What a missing `ssh-keygen` is reported as. `Command::status()` answers
/// one with a bare `No such file or directory` that does not say which file
/// — on a stripped image that reading cost an afternoon (acs-cu8), so the
/// tool is named here.
const NEEDS_KEYGEN: &str =
    "ssh-keygen must be on PATH: the release signature is made and checked with it (acs-o9v)";

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
    /// (acs-o9v), for `ACS_RELEASE_KEY`. A key per server, made on first
    /// use, so one test's key is never another's.
    pub fn release_key(&self) -> String {
        self.signing_key();
        std::fs::read_to_string(self.root.path().join("signer.pub"))
            .unwrap()
            .trim()
            .to_string()
    }

    /// The private half's path, making the pair if it is not there yet.
    fn signing_key(&self) -> PathBuf {
        let key = self.root.path().join("signer");
        if !key.is_file() {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
                .arg(&key)
                .status()
                .expect(NEEDS_KEYGEN);
            assert!(st.success());
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
        self.put(&format!("/download/v{version}/{name}.tar.gz"), data);
        let versioned = format!("/download/v{version}/SHA256SUMS");
        self.put(&versioned, sums.clone());
        self.publish_signature(&versioned, &sums, None);
        if latest {
            self.put("/latest/download/SHA256SUMS", sums.clone());
            self.publish_signature("/latest/download/SHA256SUMS", &sums, None);
        }
    }
}
