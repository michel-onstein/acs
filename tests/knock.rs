//! Port knocking in the dial (KNOCK_DESIGN §3, §5, §7, §8, acs-8l1v.4).
//!
//! A fake knockd stands in front of the fake remote: three UDP sockets on
//! loopback watch for the sequence, in order, and only a completed one
//! opens the "firewall" — a file the fake transport checks before it lets
//! the dial through, and which closes again after a while as knockd's
//! `cmd_timeout` does. A dial that did not knock gets ssh's own
//! `Connection timed out`. UDP rather than TCP, because a TCP knock on
//! loopback is never seen by a listener on macOS: the socket is closed in
//! `SYN_SENT` before the handshake completes (KNOCK_DESIGN §11.2).

mod common;

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acs::testutil::TempDir;
use common::*;

/// How long the fake firewall stays open after a sequence: knockd's
/// `cmd_timeout`. Shorter than the 2 s a knock is reused for, so a dial
/// made later than that had to knock again to get in.
const OPEN_FOR: Duration = Duration::from_millis(1500);

/// A fake knockd and the firewall it opens.
struct Knockd {
    dir: TempDir,
    ports: [u16; 3],
    /// The knocks seen, as indices into `ports`, in the order they came.
    hits: Arc<Mutex<Vec<usize>>>,
    /// Sequences completed in order.
    opened: Arc<Mutex<usize>>,
    stop: Arc<AtomicBool>,
}

impl Knockd {
    fn new() -> Knockd {
        let dir = TempDir::new();
        let socks: Vec<UdpSocket> = (0..3)
            .map(|_| {
                let s = UdpSocket::bind("127.0.0.1:0").unwrap();
                s.set_nonblocking(true).unwrap();
                s
            })
            .collect();
        let ports = [0, 1, 2].map(|i| socks[i].local_addr().unwrap().port());
        let k = Knockd {
            dir,
            ports,
            hits: Arc::default(),
            opened: Arc::default(),
            stop: Arc::default(),
        };
        let (hits, opened, stop) = (k.hits.clone(), k.opened.clone(), k.stop.clone());
        let open = k.open_file();
        std::thread::spawn(move || {
            // knockd's progress for the one source address there is.
            let mut next = 0;
            while !stop.load(Ordering::Relaxed) {
                let mut idle = true;
                for (i, s) in socks.iter().enumerate() {
                    while s.recv_from(&mut [0u8; 16]).is_ok() {
                        idle = false;
                        hits.lock().unwrap().push(i);
                        // A hit on the next port advances; any other starts
                        // over, as knockd does.
                        next = if i == next {
                            next + 1
                        } else {
                            usize::from(i == 0)
                        };
                        if next == 3 {
                            next = 0;
                            *opened.lock().unwrap() += 1;
                            std::fs::write(&open, b"").unwrap();
                            let open = open.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(OPEN_FOR);
                                let _ = std::fs::remove_file(open);
                            });
                        }
                    }
                }
                if idle {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        });
        k
    }

    /// The file whose presence is the firewall open.
    fn open_file(&self) -> PathBuf {
        self.dir.path().join("open")
    }

    /// The sequence knockd watches for, as `--knock` takes it.
    fn spec(&self) -> String {
        let [a, b, c] = self.ports;
        format!("{a}:udp,{b}:udp,{c}:udp")
    }

    /// The same ports in the wrong order: a sequence that never opens.
    fn wrong_spec(&self) -> String {
        let [a, b, c] = self.ports;
        format!("{c}:udp,{b}:udp,{a}:udp")
    }

    fn opened(&self) -> usize {
        *self.opened.lock().unwrap()
    }

    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    /// Wait for `n` sequences to have completed.
    fn wait_opened(&self, n: usize) {
        wait_until(
            T,
            || self.opened() >= n,
            || format!("{} sequences opened, waiting for {n}", self.opened()),
        );
    }

    /// `--transport-cmd` for `remote` behind this firewall: through while
    /// it is open, ssh's connect timeout otherwise.
    fn gate(&self, remote: &Remote) -> String {
        let path = self.dir.path().join("gate.sh");
        write_executable(
            &path,
            format!(
                "#!/bin/sh\n\
                 [ -f '{open}' ] && exec {transport} \"$1\"\n\
                 echo 'ssh: connect to host gated port 22: Connection timed out' >&2\n\
                 exit 255\n",
                open = self.open_file().display(),
                transport = remote.transport(),
            ),
        );
        path.display().to_string()
    }

    /// An `ssh` for `ACS_SSH` that only answers `ssh -G`, as ssh would for
    /// a host at 127.0.0.1 — through a ProxyJump once [`Knockd::proxied`]
    /// says so. The dial itself goes through `--transport-cmd`.
    fn ssh_g(&self) -> String {
        let path = self.dir.path().join("ssh");
        write_executable(
            &path,
            format!(
                "#!/bin/sh\n\
                 [ \"$1\" = -G ] || exit 2\n\
                 printf 'hostname 127.0.0.1\\nport 22\\naddressfamily inet\\n'\n\
                 [ -f '{jump}' ] && echo 'proxyjump bastion'\n\
                 exit 0\n",
                jump = self.jump_file().display(),
            ),
        );
        path.display().to_string()
    }

    fn jump_file(&self) -> PathBuf {
        self.dir.path().join("jump")
    }

    /// From now on ssh reaches the host through a ProxyJump.
    fn proxied(&self) {
        std::fs::write(self.jump_file(), b"").unwrap();
    }

    /// Assert that `text` names none of the knock's ports (§7).
    fn assert_no_ports(&self, text: &str) {
        for p in self.ports {
            for line in text.lines().filter(|l| l.contains("knock")) {
                assert!(!line.contains(&p.to_string()), "port {p} told: {line}");
            }
            for shape in [format!("{p}:udp"), format!(":{p}"), format!("{p},")] {
                assert!(!text.contains(&shape), "port {p} told:\n{text}");
            }
        }
    }
}

impl Drop for Knockd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The environment of a client knocking with `spec`, its `ssh -G` the fake.
fn env(k: &Knockd, spec: &str) -> Vec<(String, String)> {
    vec![
        ("ACS_SSH".into(), k.ssh_g()),
        ("ACS_KNOCK".into(), spec.into()),
    ]
}

/// A session on `gated` behind `k`, under -v.
fn session(k: &Knockd, remote: &Remote, spec: &str, extra: &[(&str, &str)]) -> Client {
    let gate = k.gate(remote);
    let mut args = vec!["--transport-cmd", &gate, "-v", "gated", "s"];
    args.extend_from_slice(UP_CMD);
    let env = env(k, spec);
    let mut env = refs(&env);
    env.extend_from_slice(extra);
    Client::spawn(&exe(), &args, &env)
}

/// §3.1, §7: the first connection knocks, the firewall opens, and the
/// session comes up — with a `-v` line naming the host and the count, a
/// `knocked` timing phase between the master check and the spawn, and no
/// port anywhere.
#[test]
fn the_first_connection_knocks() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let mut c = session(&k, &remote, &k.spec(), &[]);
    c.wait_session("s");
    c.wait_for("up", T);
    assert_eq!(k.opened(), 1);
    let text = c.text();
    assert!(
        text.contains("acs: knocking 127.0.0.1 (127.0.0.1): 3 knocks, 50 ms apart, then 200 ms"),
        "{text}"
    );
    let knocked = text.find("timing: first connection: knocked").expect(&text);
    let spawned = text.find("timing: first connection: ssh spawned").unwrap();
    assert!(knocked < spawned, "{text}");
    k.assert_no_ports(&text);
}

/// §3.1, §8: a redial knocks again, since the firewall has closed behind
/// the first knock by then — and is let in.
#[test]
fn a_redial_knocks_again() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let mut c = session(&k, &remote, &k.spec(), &[]);
    c.wait_session("s");
    c.wait_for("up", T);
    assert_eq!(k.opened(), 1);
    // Past the 2 s a knock is reused for, and the firewall long closed.
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!k.open_file().exists());
    let base = remote.connections();
    remote.cut_link();
    remote.wait_more_connections(base, 1, T);
    k.wait_opened(2);
    c.wait_resumed();
    let text = c.text();
    assert!(text.contains("timing: redial: knocked"), "{text}");
    k.assert_no_ports(&text);
}

/// §7: a knock that does not open the port ends as ssh's connect timeout,
/// and acs adds the hint that is the only way to ask why.
#[test]
fn a_wrong_sequence_ends_with_the_hint() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let mut c = session(&k, &remote, &k.wrong_spec(), &[]);
    assert_eq!(c.wait(T), 255);
    let text = c.text();
    assert!(text.contains("Connection timed out"), "{text}");
    assert!(
        text.contains(
            "acs: 127.0.0.1 was knocked before this dial; if it is up, the sequence or its \
             timing may not match knockd's (see knockd's log)"
        ),
        "{text}"
    );
    assert_eq!(k.opened(), 0);
    assert_eq!(k.hits(), 3, "one knock per port, and no more");
    k.assert_no_ports(&text);
}

/// §5, §8: a knock from here through a ProxyJump would open the firewall
/// to the wrong address, so it is refused — with the error code, the
/// session's and a listing's alike — unless `knock.host` says where to
/// knock, which then goes through.
#[test]
fn a_proxied_dial_refuses_to_knock_unless_told_where() {
    let k = Knockd::new();
    let remote = Remote::installed();
    k.proxied();
    let refused = "acs: knock: gated is reached through ProxyJump bastion, so a knock from \
                   here would open it to the wrong address; set knock.host to knock the jump \
                   host, or knock from the jump host's ProxyCommand";
    let mut c = session(&k, &remote, &k.spec(), &[]);
    assert_eq!(c.wait(T), 1);
    assert!(c.text().contains(refused), "{}", c.text());
    let out = output_of(
        acs_cmd()
            .args(["list", "--transport-cmd", &k.gate(&remote), "gated"])
            .envs(env(&k, &k.spec())),
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(refused),
        "{out:?}"
    );
    assert_eq!(k.hits(), 0, "nothing was knocked");
    // knock.host knocks the host it names, proxied or not.
    let spec = format!("{}/host=127.0.0.1", k.spec());
    let mut c = session(&k, &remote, &spec, &[]);
    c.wait_session("s");
    c.wait_for("up", T);
    assert_eq!(k.opened(), 1);
}

/// §3.2, §8: with a knock configured, an `ssh -G` that cannot say where
/// ssh connects fails the dial with the reason; the knock is never sent to
/// a guess.
#[test]
fn no_knock_is_sent_to_a_guess() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let out = output_of(
        acs_cmd()
            .args(["list", "--transport-cmd", &k.gate(&remote), "gated"])
            .env("ACS_SSH", "/nonexistent/ssh")
            .env("ACS_KNOCK", k.spec()),
    );
    assert_eq!(out.status.code(), Some(255), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("knock: cannot tell where ssh connects for gated (ssh -G failed)"),
        "{err}"
    );
    assert_eq!(k.hits(), 0);
}

/// §3.1: a command run without a terminal knocks before its dial too.
#[test]
fn a_command_without_a_terminal_knocks_first() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let out = output_of(
        acs_cmd()
            .args([
                "--transport-cmd",
                &k.gate(&remote),
                "gated",
                "--",
                "echo hi",
            ])
            .envs(env(&k, &k.spec()))
            .stdin(std::process::Stdio::null()),
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(k.opened(), 1);
}

/// A fake `ssh` for `--ssh` with acs's master in it: `ssh -G` as for
/// 127.0.0.1, `-O check` answered where the socket is, and a dial that
/// finds the socket there joins it — no connection, so no firewall to get
/// through — while one that does not opens a connection behind `k`'s
/// firewall and becomes the master.
fn mux_ssh(k: &Knockd, remote: &Remote) -> PathBuf {
    let path = k.dir.path().join("mux-ssh");
    write_executable(
        &path,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = -G ] && {{ printf 'hostname 127.0.0.1\\nport 22\\n'; exit 0; }}\n\
             cp=\n\
             for a in \"$@\"; do case \"$a\" in ControlPath=/*) cp=${{a#ControlPath=}} ;; esac; done\n\
             case \" $* \" in *' -O '*) [ -n \"$cp\" ] && [ -e \"$cp\" ]; exit $? ;; esac\n\
             while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             shift 2\n\
             if [ -n \"$cp\" ] && [ -e \"$cp\" ]; then exec {transport} \"$1\"; fi\n\
             if [ ! -f '{open}' ]; then\n\
             echo 'ssh: connect to host gated port 22: Connection timed out' >&2; exit 255\n\
             fi\n\
             [ -n \"$cp\" ] && : > \"$cp\"\n\
             exec {transport} \"$1\"\n",
            open = k.open_file().display(),
            transport = remote.transport(),
        ),
    );
    path
}

/// `acs list -v gated` through `ssh`, with acs's master kept in `dir`.
fn list(k: &Knockd, ssh: &Path, dir: &Path) -> std::process::Output {
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", ssh.to_str().unwrap(), "-v", "gated"])
            .env("ACS_KNOCK", k.spec())
            .env("ACS_CONTROL_DIR", dir)
            .env("ACS_CONTROL_PERSIST", "300"),
    );
    assert!(out.status.success(), "{out:?}");
    out
}

/// §3.1: a call that joins acs's master opens no connection, so it does
/// not knock — not even a reused knock — while the call that started the
/// master did.
#[test]
fn a_dial_joining_acss_master_does_not_knock() {
    let k = Knockd::new();
    let remote = Remote::installed();
    let ssh = mux_ssh(&k, &remote);
    let dir = TempDir::new();
    let mux = dir.path().join("mux");
    std::fs::create_dir(&mux).unwrap();
    std::fs::set_permissions(&mux, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let first = list(&k, &ssh, &mux);
    assert_eq!(k.opened(), 1, "{first:?}");
    let err = String::from_utf8_lossy(&first.stderr);
    assert!(err.contains("acs: knocking 127.0.0.1"), "{err}");
    k.assert_no_ports(&err);
    // Past the firewall's window: only a join can get in now.
    std::thread::sleep(OPEN_FOR + Duration::from_millis(200));
    assert!(!k.open_file().exists());
    let second = list(&k, &ssh, &mux);
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(!err.contains("knock"), "{err}");
    assert_eq!((k.opened(), k.hits()), (1, 3));
}
