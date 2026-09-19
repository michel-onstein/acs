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

pub const T: Duration = Duration::from_secs(15);

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
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"));
    c
}

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

    pub fn install(&self, version: &str) {
        let dir = self.home().join(format!(".local/share/acs/{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("acs"));
        std::os::unix::fs::symlink(exe(), dir.join("acs")).unwrap();
    }

    /// Install a real copy (not a symlink): `current_exe()` must then be
    /// the versioned path, as on a real host.
    pub fn install_copy(&self, version: &str) {
        let dir = self.home().join(format!(".local/share/acs/{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("acs"));
        std::fs::copy(exe(), dir.join("acs")).unwrap();
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
                "#!/bin/sh\necho $$ >> '{pids}'\n[ -f '{delay}' ] && sleep \"$(cat '{delay}')\"\n[ -f '{silent}' ] && {{ cat '{silent}'; exec sleep 60; }}\n[ -f '{noise}' ] && cat '{noise}'\nexport HOME='{home}' ACS_SOCKET_DIR='{sock}' PATH='{fake}':\"$PATH\"\nexec /bin/sh -c \"$1\"\n",
                pids = self.pid_file().display(),
                silent = self.silent_file().display(),
                delay = self.delay_file().display(),
                noise = self.noise_file().display(),
                home = self.home().display(),
                sock = self.sockets().display(),
                fake = self.fake_bin().display(),
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
            .env("ACS_IDENTITY", "tester@local");
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
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
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
            searched: 0,
        }
    }

    /// Type bytes.
    pub fn send(&self, bytes: &[u8]) {
        sys::write_all(self.pty.as_raw_fd(), bytes).unwrap();
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
        self.put(&format!("/download/v{version}/SHA256SUMS"), sums.clone());
        if latest {
            self.put("/latest/download/SHA256SUMS", sums);
        }
    }
}
