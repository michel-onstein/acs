//! Port knocking against a **real knockd**, a real firewall and a real sshd
//! (KNOCK_DESIGN §9, §11.2, §12 step 6; acs-8l1v.6).
//!
//! `tests/knock.rs` drives the dial against a fake knockd: UDP sockets on
//! loopback and a file standing in for the firewall. What it cannot say is
//! whether the packets acs sends are ones the real knockd counts — a TCP
//! knock is a bare SYN that knockd only sees through libpcap, with
//! `tcpflags = syn` — and whether a session really outlives the rule
//! knockd removes after `cmd_timeout`, which rests on the kernel's
//! connection tracking and not on anything acs does.
//!
//! Here all of it is real. `scripts/test_linux.sh --only knockd` runs this
//! as root in a container of its own with `--cap-add NET_ADMIN`, having
//! installed `knock` (knockd), `iptables`, `openssh-server` and
//! `openssh-client`, and sets `ACS_KNOCKD_TEST=1` to say so. The test then
//! makes the host this file is about, on the container's loopback:
//!
//! - **sshd** on port 22, accepting one key for the user `knocker`, who has
//!   this acs installed where the prelude looks for it;
//! - **iptables**: on `lo`, accept `ESTABLISHED,RELATED` to port 22 and drop
//!   every other packet to it — the setup every knockd guide gives;
//! - **knockd** on `lo`, opening port 22 to the knocking address on
//!   `7000,8000:udp,9000` with `iptables -I`, and closing it again
//!   [`CMD_TIMEOUT`] seconds later.
//!
//! Loopback rather than a second network namespace, because a new
//! namespace needs `CAP_SYS_ADMIN` on top of `CAP_NET_ADMIN`; traffic to
//! 127.0.0.1 still goes through the kernel's netfilter `INPUT` chain and
//! conntrack, and libpcap on `lo` sees every packet once, so knockd, the
//! firewall and the connection tracking are the ones a remote host has.
//!
//! The test is `#[ignore]` and compiled everywhere, so a plain `cargo test`
//! counts it as ignored (acs-hdj4); asked for anywhere else it fails.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use acs::testutil::TempDir;
use common::*;

/// The sequence knockd opens port 22 on: TCP, UDP and TCP, so both kinds
/// of knock go through the real libpcap filter.
const SEQUENCE: &str = "7000,8000:udp,9000";

/// A sequence knockd does not know. None of its ports is in [`SEQUENCE`],
/// so it leaves no half-finished attempt behind for the next test.
const WRONG: &str = "7001,8001:udp,9001";

/// knockd's `cmd_timeout`: how long port 22 stays open after a knock.
/// Short, so the session test can outlast it in seconds.
const CMD_TIMEOUT: u64 = 3;

const USER: &str = "knocker";

/// The firewall rule knockd's `start_command` inserts for loopback.
const OPEN_RULE: &str = "-s 127.0.0.1 -p tcp --dport 22 -j ACCEPT";

/// The host's own rules, ahead of knockd's: established connections are
/// kept, every other packet to port 22 is dropped (KNOCK_DESIGN §9).
const ESTABLISHED_RULE: &str =
    "-A INPUT -i lo -p tcp --dport 22 -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT";
const DROP_RULE: &str = "-A INPUT -i lo -p tcp --dport 22 -j DROP";

fn run(program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"))
}

/// `iptables` with `rule`, split on spaces; whether it succeeded.
fn iptables(rule: &str) -> bool {
    let args: Vec<&str> = rule.split(' ').collect();
    run("iptables", &args).status.success()
}

fn must(program: &str, args: &[&str]) {
    let out = run(program, args);
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The knockd host, made once per run of this file.
struct Gate {
    dir: TempDir,
}

impl Gate {
    fn key(&self) -> PathBuf {
        self.dir.path().join("key")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("knockd.log")).unwrap_or_default()
    }

    /// The ssh options every dial here uses: only our key, no user config,
    /// a throwaway host key.
    fn ssh_args(&self) -> Vec<String> {
        [
            "-F",
            "/dev/null",
            "-i",
            self.key().to_str().unwrap(),
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ]
        .map(String::from)
        .to_vec()
    }

    /// Whether plain ssh, with no knock, gets through the firewall now.
    fn plain_ssh_connects(&self) -> bool {
        let mut args = self.ssh_args();
        args.extend(
            ["-o", "BatchMode=yes", "-o", "ConnectTimeout=2"]
                .iter()
                .map(|s| s.to_string()),
        );
        args.push(format!("{USER}@127.0.0.1"));
        args.push("true".into());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run("ssh", &args).status.success()
    }

    /// Whether knockd's rule for 127.0.0.1 is in the firewall.
    fn open(&self) -> bool {
        iptables(&format!("-C INPUT {OPEN_RULE}"))
    }

    /// Wait until knockd has taken its rule out again, so that each test
    /// starts with port 22 closed.
    fn wait_closed(&self) {
        acs::testutil::wait_until(
            Duration::from_secs(CMD_TIMEOUT + 5),
            || !self.open(),
            || format!("knockd never closed port 22:\n{}", self.log()),
        );
    }

    /// `acs` on a pty with the gate's ssh options, then `args`.
    fn acs(&self, args: &[&str], env: &[(&str, &str)]) -> Client {
        let mut full = self.ssh_args();
        full.extend(args.iter().map(|s| s.to_string()));
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        Client::spawn(&exe(), &full, env)
    }
}

fn gate() -> &'static Gate {
    static GATE: OnceLock<Gate> = OnceLock::new();
    GATE.get_or_init(set_up)
}

fn set_up() -> Gate {
    assert!(
        cfg!(any(target_os = "linux", target_os = "android"))
            && std::env::var_os("ACS_KNOCKD_TEST").is_some(),
        "needs Linux, a container of its own with CAP_NET_ADMIN, knockd, \
         iptables and sshd, and ACS_KNOCKD_TEST=1: this test runs under \
         scripts/test_linux.sh --only knockd"
    );
    let g = Gate {
        dir: TempDir::new(),
    };
    let dir = g.dir.path();

    // The user, with a usable account (adduser -D locks it with `!`, which
    // sshd refuses even for a key) and our key.
    let _ = run("adduser", &["-D", "-s", "/bin/sh", USER]);
    must(
        "sed",
        &["-i", &format!("s/^{USER}:!/{USER}:*/"), "/etc/shadow"],
    );
    must(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            g.key().to_str().unwrap(),
        ],
    );
    let home = PathBuf::from(format!("/home/{USER}"));
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::copy(dir.join("key.pub"), ssh_dir.join("authorized_keys")).unwrap();
    // This acs, where the prelude looks for the client's own version, so
    // the dial is one connection and no first-contact install.
    let install = home.join(format!(".local/share/acs/{}", acs::VERSION));
    std::fs::create_dir_all(&install).unwrap();
    copy_out_of_process(&exe(), &install.join("acs"));
    must(
        "chown",
        &["-R", &format!("{USER}:{USER}"), home.to_str().unwrap()],
    );
    must("chmod", &["700", ssh_dir.to_str().unwrap()]);
    must("chmod", &["755", install.join("acs").to_str().unwrap()]);

    // sshd, before the firewall goes up.
    must("ssh-keygen", &["-A"]);
    must("/usr/sbin/sshd", &[]);
    assert!(g.plain_ssh_connects(), "sshd does not answer at all");

    // The firewall: keep established connections, drop the rest.
    for rule in [ESTABLISHED_RULE, DROP_RULE] {
        assert!(iptables(rule), "iptables {rule} failed");
    }
    assert!(!g.plain_ssh_connects(), "the firewall lets ssh through");

    // knockd, as its own guide configures it.
    let conf = dir.join("knockd.conf");
    std::fs::write(
        &conf,
        format!(
            "[options]\n\
             \x20   Interface = lo\n\
             \x20   LogFile = {log}\n\
             [openSSH]\n\
             \x20   sequence = {SEQUENCE}\n\
             \x20   seq_timeout = 5\n\
             \x20   tcpflags = syn\n\
             \x20   start_command = /usr/sbin/iptables -I INPUT {OPEN_RULE}\n\
             \x20   cmd_timeout = {CMD_TIMEOUT}\n\
             \x20   stop_command = /usr/sbin/iptables -D INPUT {OPEN_RULE}\n",
            log = dir.join("knockd.log").display(),
        ),
    )
    .unwrap();
    must("knockd", &["-d", "-c", conf.to_str().unwrap()]);
    acs::testutil::wait_until(
        Duration::from_secs(5),
        || g.log().contains("listening on lo"),
        || format!("knockd did not start:\n{}", g.log()),
    );
    g
}

/// How many times acs knocked, by its `-v` lines.
fn knocks(text: &str) -> usize {
    text.matches("acs: knocking 127.0.0.1").count()
}

/// The ports must never show in what acs prints (KNOCK_DESIGN §7): not on
/// a line about the knock, and not in the shapes a sequence is written in.
fn assert_no_ports(text: &str, spec: &str) {
    for port in spec.split(',').map(|p| p.split(':').next().unwrap()) {
        for line in text.lines().filter(|l| l.contains("knock")) {
            assert!(!line.contains(port), "port {port} told: {line}");
        }
        for shape in [
            format!("{port}:udp"),
            format!("{port},"),
            format!(",{port}"),
        ] {
            assert!(!text.contains(&shape), "port {port} told:\n{text}");
        }
    }
}

/// The right sequence through `--knock` opens the real firewall, and the
/// session comes up over real ssh — after a check that plain ssh without
/// a knock cannot.
#[test]
#[ignore = "needs a Linux container with CAP_NET_ADMIN and knockd: run by scripts/test_linux.sh --only knockd"]
fn the_right_sequence_gets_through() {
    let g = gate();
    g.wait_closed();
    assert!(!g.plain_ssh_connects(), "port 22 is open without a knock");
    let dest = format!("{USER}@127.0.0.1");
    let mut c = g.acs(
        &["--knock", SEQUENCE, "-v", &dest, "right", "--", UP_SH],
        &[],
    );
    c.wait_session("right");
    c.wait_for("up", T);
    let text = c.text();
    assert!(
        text.contains("acs: knocking 127.0.0.1 (127.0.0.1): 3 knocks, 50 ms apart, then 200 ms"),
        "{text}"
    );
    assert!(text.contains("timing: first connection: knocked"), "{text}");
    assert_no_ports(&text, SEQUENCE);
    assert!(
        g.log().contains("127.0.0.1: openSSH: OPEN SESAME"),
        "knockd did not open for acs's knock:\n{}",
        g.log()
    );
}

/// A sequence knockd does not know leaves port 22 shut: ssh times out
/// against the real firewall, acs exits as for an unreachable host and
/// adds the hint that is the only way to ask why.
#[test]
#[ignore = "needs a Linux container with CAP_NET_ADMIN and knockd: run by scripts/test_linux.sh --only knockd"]
fn a_wrong_sequence_fails_with_the_hint() {
    let g = gate();
    g.wait_closed();
    let opened = g.log().matches("OPEN SESAME").count();
    let dest = format!("{USER}@127.0.0.1");
    let mut c = g.acs(
        &[
            "--knock",
            WRONG,
            "-o",
            "ConnectTimeout=3",
            "-v",
            &dest,
            "wrong",
            "--",
            UP_SH,
        ],
        &[],
    );
    assert_eq!(c.wait(T), 255, "{}", c.text());
    let text = c.text();
    assert!(text.contains("timed out"), "{text}");
    assert!(
        text.contains(
            "acs: 127.0.0.1 was knocked before this dial; if it is up, the sequence or its \
             timing may not match knockd's (see knockd's log)"
        ),
        "{text}"
    );
    assert_eq!(knocks(&text), 1, "{text}");
    assert_no_ports(&text, WRONG);
    assert_eq!(
        g.log().matches("OPEN SESAME").count(),
        opened,
        "knockd opened for a wrong sequence:\n{}",
        g.log()
    );
}

/// KNOCK_DESIGN §9: with the `ESTABLISHED,RELATED` rule, a session outlives
/// knockd closing the port behind it. The knock comes from the
/// configuration file here — an alias with the mapping form — and the
/// session is still answering well past `cmd_timeout`, with the rule gone
/// and a fresh connection refused entry, and acs never had to redial.
#[test]
#[ignore = "needs a Linux container with CAP_NET_ADMIN and knockd: run by scripts/test_linux.sh --only knockd"]
fn a_session_outlives_cmd_timeout() {
    let g = gate();
    g.wait_closed();
    let cfg = TempDir::new();
    let env = config_env(
        cfg.path(),
        &format!(
            "aliases:\n\
             \x20 gated:\n\
             \x20   - host: 127.0.0.1\n\
             \x20     user: {USER}\n\
             \x20     reachability_check: false\n\
             \x20     knock:\n\
             \x20       sequence: {SEQUENCE}\n\
             \x20       delay: 100ms\n\
             \x20       wait: 300ms\n"
        ),
    );
    let mut c = g.acs(
        &[
            "-v",
            "gated",
            "long",
            "--",
            "/bin/sh -c 'echo up; while read l; do echo \"got:$l\"; done'",
        ],
        &refs(&env),
    );
    c.wait_session("long");
    c.wait_for("up", T);
    assert!(
        c.text()
            .contains("acs: knocking 127.0.0.1 (127.0.0.1): 3 knocks, 100 ms apart, then 300 ms"),
        "{}",
        c.text()
    );
    c.send(b"before\r");
    c.wait_for("got:before", T);

    // knockd's stop_command runs CMD_TIMEOUT seconds after the knock. Wait
    // for it, then a little more, so the session's packets have been
    // carried by the ESTABLISHED rule alone for a while.
    let started = Instant::now();
    g.wait_closed();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        g.log().contains("openSSH: command timeout"),
        "knockd did not close the port:\n{}",
        g.log()
    );
    assert!(
        !g.plain_ssh_connects(),
        "a new connection got through after cmd_timeout"
    );

    c.send(b"after\r");
    c.wait_for("got:after", T);
    let text = c.text();
    assert_eq!(knocks(&text), 1, "acs knocked again:\n{text}");
    assert!(!text.contains("timing: redial"), "acs redialled:\n{text}");
    assert_no_ports(&text, SEQUENCE);
    assert!(
        started.elapsed() >= Duration::from_secs(CMD_TIMEOUT),
        "the session was not kept past cmd_timeout"
    );
}
