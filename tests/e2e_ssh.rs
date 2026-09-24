//! End-to-end over real ssh against a container host (acs-5v9.24). Run by
//! `scripts/e2e_ssh.sh`, which starts the host and sets `ACS_E2E_*`;
//! skipped otherwise.
//!
//! The tests run one at a time in name order, and `e2e_01` must be first
//! (it checks the first-contact install), so numbers are zero-padded:
//! `e2e_10` would otherwise sort before `e2e_1`.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::*;

struct Host {
    client: String,
    key: String,
    port: String,
    container: String,
}

fn host() -> Option<Host> {
    let v = |k: &str| std::env::var(k).ok();
    Some(Host {
        client: v("ACS_E2E_CLIENT")?,
        key: v("ACS_E2E_KEY")?,
        port: v("ACS_E2E_PORT")?,
        container: v("ACS_E2E_CONTAINER")?,
    })
}

impl Host {
    /// ssh options: only our key, no user config, throwaway host key.
    fn ssh_args(&self) -> Vec<String> {
        [
            "-F",
            "/dev/null",
            "-i",
            &self.key,
            "-p",
            &self.port,
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn client(&self, session: &str, cmd: &str, env: &[(&str, &str)]) -> Client {
        let mut args = self.ssh_args();
        args.push("dev@127.0.0.1".into());
        args.push(session.into());
        if !cmd.is_empty() {
            args.extend(["--", "/bin/sh", "-c", cmd].map(String::from));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Client::spawn(std::path::Path::new(&self.client), &args, env)
    }

    fn docker(&self, args: &[&str]) {
        let st = Command::new("docker").args(args).status().unwrap();
        assert!(st.success(), "docker {args:?}");
    }
}

const TICKER: &str = "i=0; while true; do i=$((i+1)); printf '#%d#\\n' $i; sleep 0.02; done";

fn numbers(text: &str) -> Vec<u64> {
    let mut clean = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("\x1b7") {
        clean.push_str(&rest[..i]);
        rest = match rest[i..].find("\x1b8") {
            Some(j) => &rest[i + j + 2..],
            None => "",
        };
    }
    clean.push_str(rest);
    clean.split('#').filter_map(|p| p.parse().ok()).collect()
}

fn assert_consecutive(text: &str) {
    let n = numbers(text);
    assert!(n.len() > 10, "too little output");
    for w in n.windows(2) {
        assert_eq!(w[1], w[0] + 1, "lost or repeated output");
    }
}

/// A local port with nothing on it: taken from the kernel and let go again,
/// as the host's ssh port is, so two runs at once never pick the same one.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Whatever the far end of `127.0.0.1:port` says first; `None` while nothing
/// is listening there, or while what listens says nothing.
fn greeting(port: u16) -> Option<String> {
    use std::io::Read;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(String::from_utf8_lossy(&buf[..n]).trim().to_string()),
    }
}

/// [`greeting`], waiting for the forward to be bound and carrying bytes.
fn wait_greeting(port: u16, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(g) = greeting(port) {
            return g;
        }
        assert!(
            Instant::now() < deadline,
            "nothing answered on 127.0.0.1:{port} within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn e2e_01_first_contact_installs_and_attaches() {
    let Some(h) = host() else { return };
    let mut c = h.client("first", "echo up-and-running; sleep 60", &[]);
    c.wait_for("installing acs", T);
    c.wait_for("(Linux aarch64)", T);
    c.wait_for("installed acs", T);
    c.wait_for("up-and-running", T);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    c.wait_for("reattach with: acs dev@127.0.0.1 first", T);
    eprintln!("VERIFIED install over ssh from the macOS complete build");
}

#[test]
fn e2e_02_identity_file_is_required() {
    let Some(h) = host() else { return };
    // Same options but no -i: this host only accepts the generated key.
    let args = [
        "-F",
        "/dev/null",
        "-p",
        &h.port,
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "dev@127.0.0.1",
        "nokey",
    ];
    let mut c = Client::spawn(std::path::Path::new(&h.client), &args, &[]);
    assert_eq!(c.wait(T), 255);
    eprintln!("VERIFIED -i selects the key (without it: exit 255)");
}

#[test]
fn e2e_03_vim_uses_the_alternate_screen_and_exits_cleanly() {
    let Some(h) = host() else { return };
    let mut c = h.client("vim", "vim -u NONE -N", &[]);
    c.wait_for("\x1b[?1049h", T);
    std::thread::sleep(Duration::from_millis(500));
    c.send(b":q\r");
    assert_eq!(c.wait(T), 0);
    c.wait_for("\x1b[?1049l", T);
    assert!(c.echo_on());
    eprintln!("VERIFIED vim: alternate screen in and out, exit status 0");
}

#[test]
fn e2e_04_osc52_and_hyperlinks_pass_through_verbatim() {
    let Some(h) = host() else { return };
    let mut c = h.client(
        "osc",
        "printf '\\033]52;c;aGVsbG8=\\007\\033]8;;https://x.test\\033\\\\link\\033]8;;\\033\\\\'; echo; echo done; sleep 30",
        &[],
    );
    c.wait_for("done", T);
    let text = c.text();
    assert!(text.contains("\x1b]52;c;aGVsbG8=\x07"), "OSC 52 altered");
    assert!(
        text.contains("\x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\"),
        "OSC 8 altered"
    );
    eprintln!("VERIFIED OSC 52 clipboard and OSC 8 hyperlinks byte-exact");
}

#[test]
fn e2e_05_kitty_keyboard_protocol_command_key() {
    let Some(h) = host() else { return };
    let mut c = h.client("kitty", "printf '\\033[>1u'; echo kitty-on; sleep 60", &[]);
    c.wait_for("kitty-on", T);
    c.send(b"\x1b[93;5u\x1b[93;5ud");
    assert_eq!(c.wait(T), 0);
    c.wait_for("\x1b[<1u", T);
    eprintln!("VERIFIED Ctrl-] Ctrl-] d in kitty encoding detaches; kitty flags popped");
}

#[test]
fn e2e_06_mouse_reports_reach_the_program() {
    let Some(h) = host() else { return };
    let mut c = h.client(
        "mouse",
        "stty raw -echo; printf '\\033[?1000h'; echo mouse-on; od -An -tx1 -N 6",
        &[],
    );
    c.wait_for("mouse-on", T);
    c.send(b"\x1b[M !!");
    c.wait_for("21", T);
    let text = c.text();
    let words: Vec<&str> = text[text.find("mouse-on").unwrap()..]
        .split_whitespace()
        .collect();
    assert!(
        words
            .windows(6)
            .any(|w| w == ["1b", "5b", "4d", "20", "21", "21"]),
        "{words:?}"
    );
    eprintln!("VERIFIED X10 mouse report delivered byte-exact");
}

#[test]
fn e2e_07_killed_connection_resumes_without_loss() {
    let Some(h) = host() else { return };
    let mut c = h.client("drop", TICKER, &[("ACS_BACKOFF_MS", "300")]);
    c.wait_for("#30#", T);
    // Kill the sshd process serving the session (not the listener).
    h.docker(&[
        "exec",
        &h.container,
        "sh",
        "-c",
        // `[s]` keeps the pattern from matching this shell's own command
        // line (pkill -f would kill it first).
        "pkill -f '[s]shd-session: dev@' || pkill -f '[s]shd: dev@'",
    ]);
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 100), Duration::from_secs(30));
    assert_consecutive(&c.text());
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!("VERIFIED killed ssh connection: reconnect with no loss");
}

#[test]
fn e2e_08_frozen_host_is_detected_and_resumed() {
    let Some(h) = host() else { return };
    let mut c = h.client(
        "freeze",
        TICKER,
        &[
            ("ACS_BACKOFF_MS", "500"),
            ("ACS_DEAD_MS", "3000"),
            ("ACS_PING_MS", "1000"),
        ],
    );
    c.wait_for("#30#", T);
    h.docker(&["pause", &h.container]);
    let t0 = Instant::now();
    c.wait_for("reconnecting", Duration::from_secs(20));
    let detected = t0.elapsed();
    std::thread::sleep(Duration::from_secs(4));
    h.docker(&["unpause", &h.container]);
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 100), Duration::from_secs(60));
    assert_consecutive(&c.text());
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!("VERIFIED frozen host: dead link detected in {detected:?}, resumed with no loss");
}

#[test]
fn e2e_09_list_shows_the_sessions() {
    let Some(h) = host() else { return };
    let mut args = vec!["list".to_string()];
    args.extend(h.ssh_args());
    args.push("dev@127.0.0.1".into());
    // No configuration: not the files of whoever runs the tests.
    let out = Command::new(&h.client)
        .args(&args)
        .env("XDG_CONFIG_HOME", NO_CONFIG)
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
        .env("ACS_CONTROL_PERSIST", NO_MASTER)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}{err}");
    assert!(text.contains("first"), "{text}{err}");
    eprintln!("VERIFIED acs list <host> over ssh:\n{text}");
}

#[test]
fn e2e_09b_list_without_a_host_asks_every_alias() {
    let Some(h) = host() else { return };
    // `box` is the container; `gone` is a documentation address no ping
    // reaches.
    let dir = acs::testutil::TempDir::new();
    let env = config_env(
        dir.path(),
        "aliases:\n  box:\n    - host: 127.0.0.1\n      user: dev\n  gone:\n    - host: 192.0.2.1\n",
    );
    let mut args = vec!["list".to_string()];
    args.extend(h.ssh_args());
    let out = Command::new(&h.client)
        .args(&args)
        .envs(env)
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
        .env("ACS_NO_UPDATE_CHECK", "1")
        .env("ACS_CONTROL_PERSIST", NO_MASTER)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(255), "{text}{err}");
    // Tests run in name order: e2e_01 left its session there.
    let row = text
        .lines()
        .find(|l| l.contains(" first "))
        .unwrap_or_else(|| panic!("no session listed:\n{text}{err}"));
    assert!(row.starts_with("box "), "{text}");
    assert!(
        err.contains("acs: gone: no host for 'gone' is reachable"),
        "{err}"
    );
    eprintln!("VERIFIED acs list over ssh (BatchMode):\n{text}{err}");
}

#[test]
fn e2e_10_user_at_alias_logs_in_as_that_user() {
    let Some(h) = host() else { return };
    // The entry's own user has no key on the host: only the override works.
    let cfg = acs::testutil::TempDir::new();
    let env = config_env(
        cfg.path(),
        "aliases:\n  box:\n    - host: 127.0.0.1\n      user: nobody\n      reachability_check: false\n",
    );
    let mut args = h.ssh_args();
    args.extend(["-v", "dev@box", "alias", "--"].map(String::from));
    args.extend(["/bin/sh", "-c", "echo \"as-$(id -un)\"; sleep 60"].map(String::from));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut c = Client::spawn(std::path::Path::new(&h.client), &args, &env);
    c.wait_for("dev@box: logging in as dev, from the command line", T);
    c.wait_for("as-dev", T);
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!("VERIFIED dev@<alias> logs in as dev over ssh");
}

#[test]
fn e2e_11_identity_file_from_the_configuration() {
    let Some(h) = host() else { return };
    // The key only under a HOME of our own, reached as ~/.ssh/id_box: acs
    // expands the ~. `keyed` names it on its host entry, over a missing
    // alias key; `plain` names it for the whole alias. No -i is given.
    let home = acs::testutil::TempDir::new();
    let key = home.path().join(".ssh/id_box");
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::copy(&h.key, &key).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let cfg = acs::testutil::TempDir::new();
    let entry = "{host: 127.0.0.1, user: dev, reachability_check: false";
    let mut env = config_env(
        cfg.path(),
        &format!(
            "aliases:\n  keyed:\n    identity_file: /nonexistent/acs-key\n    hosts:\n      - {entry}, identity_file: ~/.ssh/id_box}}\n  plain:\n    identity_file: ~/.ssh/id_box\n    hosts: [{entry}}}]\n"
        ),
    );
    env.push(("HOME".into(), home.path().display().to_string()));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    for alias in ["keyed", "plain"] {
        let mut args: Vec<String> = [
            "-F",
            "/dev/null",
            "-p",
            &h.port,
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ]
        .map(String::from)
        .to_vec();
        args.extend(
            [
                "-v",
                alias,
                "key",
                "--",
                "/bin/sh",
                "-c",
                "echo key-ok; sleep 60",
            ]
            .map(String::from),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut c = Client::spawn(std::path::Path::new(&h.client), &args, &env);
        c.wait_for(&format!("{alias}: identity_file ~/.ssh/id_box ("), T);
        c.wait_for("key-ok", T);
        c.send(&command(b'x'));
        c.wait(T);
    }
    eprintln!("VERIFIED identity_file (per host over the alias's, and per alias) over ssh");
}

#[test]
fn e2e_12_ctrl_l_after_a_reattach_and_a_resume() {
    let Some(h) = host() else { return };
    // Every byte the program receives, as `in:xx`.
    let reporter = "stty raw -echo; echo ready; \
        while :; do b=$(dd bs=1 count=1 2>/dev/null | od -An -tx1); printf 'in:%s\\n' $b; done";
    let mut c = h.client("redraw", reporter, &[]);
    c.wait_for("ready", T);
    c.send(b"z");
    c.wait_for("in:7a", T);
    assert!(!c.text().contains("in:0c"), "sent to a new session");
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let mut c = h.client("redraw", "", &[("ACS_BACKOFF_MS", "300")]);
    c.wait_for("in:0c", T);
    h.docker(&[
        "exec",
        &h.container,
        "sh",
        "-c",
        "pkill -f '[s]shd-session: dev@' || pkill -f '[s]shd: dev@'",
    ]);
    c.wait_for("in:0c", Duration::from_secs(30));
    c.send(b"z");
    c.wait_for("in:7a", T);
    assert_eq!(c.text().matches("in:0c").count(), 2, "{}", c.text());
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!("VERIFIED Ctrl-L once after a re-attach and once after a resume over ssh");
}

#[test]
fn e2e_13_plain_host_picks_a_detached_session_from_the_menu() {
    let Some(h) = host() else { return };
    let echo = |name: &str| format!("echo ready-{name}; while read l; do echo got-{name}:$l; done");
    for name in ["menu-a", "menu-b"] {
        let mut c = h.client(name, &echo(name), &[]);
        c.wait_for(&format!("ready-{name}"), T);
        c.send(&command(b'd'));
        assert_eq!(c.wait(T), 0);
    }
    let mut args = vec!["-v".to_string()];
    args.extend(h.ssh_args());
    args.push("dev@127.0.0.1".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut c = Client::spawn(std::path::Path::new(&h.client), &args, &[]);
    c.wait_for("\x1b[?1049h", T);
    c.wait_for("acs: detached sessions on dev@127.0.0.1", T);
    c.wait_for("menu-b ", T);
    // The number of a session's row, from the last screen drawn.
    let row = |c: &Client, name: &str| -> usize {
        let text = c.text();
        let screen = &text[text.rfind("\x1b[Hacs: ").unwrap()..];
        let line = screen
            .split("\r\n")
            .find(|l| l.contains(&format!(" {name} ")))
            .unwrap_or_else(|| panic!("no {name} in {screen:?}"));
        let line = line.trim_start_matches("\x1b[7m");
        line[2..3].parse().unwrap()
    };
    // End menu-b: the cursor down to it, x, y.
    let b = row(&c, "menu-b");
    c.send(&vec![b'j'; b - 1]);
    c.send(b"x");
    c.wait_for("end session 'menu-b'?", T);
    c.send(b"y");
    c.wait_for("session 'menu-b' ended", T);
    // Attach menu-a by its number.
    let a = row(&c, "menu-a");
    c.send(a.to_string().as_bytes());
    c.wait_for("\x1b[?1049l", T);
    c.wait_for("\x1b[H\x1b[J", T);
    c.send(b"hi\r");
    // menu-a existed, so the attach sends Ctrl-L first (redraw_on_reconnect).
    c.wait_for("got-menu-a:\x0chi", T);
    // One ssh connection from the list to the attach (acs-68z).
    let text = c.text();
    let running: Vec<&str> = text
        .split("\r\n")
        .filter(|l| l.contains("acs: running "))
        .collect();
    assert_eq!(running.len(), 1, "{running:?}");
    assert!(running[0].contains("_proxy --pick"), "{running:?}");
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!("VERIFIED plain acs <host>: menu, x and the attach over one ssh connection");
}

/// acs-8kv: `-L` against a real sshd. `ssh::argv` emits the forward for
/// `Call::Session` alone, so that `acs list` and the install side calls do
/// not each try to bind the user's local port (DESIGN §7.1); the unit tests
/// read that out of the argv, and only this one watches what ssh does with
/// it. The container's sshd is set to `AllowTcpForwarding yes` for it
/// (scripts/e2e/Dockerfile) — Alpine ships `no`.
#[test]
fn e2e_14_a_local_forward_is_the_session_connections_alone() {
    let Some(h) = host() else { return };
    let port = free_port();
    // Back to the container's own sshd, so what comes out of the forward is
    // the host's real banner rather than something the test planted.
    let spec = format!("{port}:127.0.0.1:22");
    let mut args = vec!["-L".to_string(), spec.clone()];
    args.extend(h.ssh_args());
    args.extend(["dev@127.0.0.1", "fwd", "--", "/bin/sh", "-c"].map(String::from));
    args.push(TICKER.to_string());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut c = Client::spawn(
        std::path::Path::new(&h.client),
        &argv,
        &[("ACS_BACKOFF_MS", "300")],
    );
    c.wait_for("#10#", T);
    let banner = wait_greeting(port, T);
    assert!(
        banner.starts_with("SSH-2.0"),
        "through the forward: {banner:?}"
    );

    // The same -L handed to `acs list <host>` — a Call::Side call — while
    // the session's ssh holds the port. Gated off, that ssh never asks for
    // the forward; ungated it would find the port taken and complain about
    // it by number ("bind: Address already in use", "cannot listen to
    // port: N", "Could not request local forwarding").
    let mut largs = vec!["list".to_string(), "-L".into(), spec.clone()];
    largs.extend(h.ssh_args());
    largs.push("dev@127.0.0.1".into());
    let out = Command::new(&h.client)
        .args(&largs)
        .env("XDG_CONFIG_HOME", NO_CONFIG)
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
        .env("ACS_NO_UPDATE_CHECK", "1")
        .env("ACS_CONTROL_PERSIST", NO_MASTER)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}{err}");
    assert!(text.contains("fwd"), "{text}{err}");
    assert!(
        !err.contains(&port.to_string()),
        "the side call asked for the forward:\n{err}"
    );
    assert!(
        !err.to_lowercase().contains("forward"),
        "the side call asked for the forward:\n{err}"
    );
    // ... and it left the session's own forward alone.
    let after = wait_greeting(port, T);
    assert!(after.starts_with("SSH-2.0"), "after acs list: {after:?}");

    // The argv is rebuilt per dial, so a redial rebinds the port.
    h.docker(&[
        "exec",
        &h.container,
        "sh",
        "-c",
        "pkill -f '[s]shd-session: dev@' || pkill -f '[s]shd: dev@'",
    ]);
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 100), Duration::from_secs(30));
    let again = wait_greeting(port, T);
    assert!(again.starts_with("SSH-2.0"), "after the redial: {again:?}");
    c.send(&command(b'x'));
    c.wait(T);
    eprintln!(
        "VERIFIED -L over ssh: {banner} through 127.0.0.1:{port}, \
         no forward on the acs list side call, rebound after a redial"
    );
}

/// The socket the master listens on. There is one per destination, so the
/// directory names it.
fn one_socket(dir: &std::path::Path) -> std::path::PathBuf {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.pop().unwrap()
}

/// `acs: timing: <what>: <phase> … (N ms total)` out of a client's output.
fn total_ms(text: &str, what: &str, phase: &str) -> Option<u64> {
    let head = format!("timing: {what}: {phase} ");
    let line = text.lines().find(|l| l.contains(&head))?;
    line.split('(').nth(1)?.split(' ').next()?.parse().ok()
}

/// acs-9n3, over real ssh — the one thing a fake transport cannot test.
///
/// Three claims, in order: a detached session leaves a master behind and
/// the next `acs` rides it rather than handshaking again; the socket it
/// rides is a `0600` file in a `0700` directory of this user's; and a lost
/// link takes the master down with it, because the master's own connection
/// is the one that failed.
#[test]
fn e2e_15_a_shared_master_carries_the_second_connection_and_dies_with_the_link() {
    use std::os::unix::fs::PermissionsExt;
    let Some(h) = host() else { return };
    let dir = acs::testutil::TempDir::new();
    let mux = dir.path().join("mux");
    let dir_s = mux.to_str().unwrap().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("ACS_CONTROL_DIR", &dir_s),
        ("ACS_CONTROL_PERSIST", "300"),
        ("ACS_BACKOFF_MS", "300"),
    ];

    // `ssh -O <op>` against whatever master is on `sock`.
    let control = |sock: &std::path::Path, op: &str| -> std::process::Output {
        let mut args = vec!["-o".to_string(), format!("ControlPath={}", sock.display())];
        args.extend(h.ssh_args());
        args.extend(["-O".to_string(), op.to_string(), "--".into()]);
        args.push("dev@127.0.0.1".into());
        Command::new("ssh").args(&args).output().unwrap()
    };

    // A first connection: it starts the master.
    let mut args = vec!["-v".to_string()];
    args.extend(h.ssh_args());
    args.extend(["dev@127.0.0.1", "mux", "--", "/bin/sh", "-c"].map(String::from));
    args.push(TICKER.to_string());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut c = Client::spawn(std::path::Path::new(&h.client), &argv, &env);
    c.wait_for("#10#", T);
    let first = c.text();
    assert!(first.contains("shared ssh master at"), "{first}");

    let sock = one_socket(&mux);
    assert_eq!(
        std::fs::metadata(&mux).unwrap().permissions().mode() & 0o777,
        0o700,
        "the directory a control socket sits in"
    );
    assert_eq!(
        std::fs::metadata(&sock).unwrap().permissions().mode() & 0o077,
        0,
        "a control socket is a shell on the far end: {sock:?}"
    );

    // Detached, the master stays: that is the whole point — the reattach
    // pays no handshake.
    c.send(&command(b'd'));
    c.wait(T);
    let check = control(&sock, "check");
    assert!(
        check.status.success(),
        "no master after detaching: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let pid = String::from_utf8_lossy(&check.stderr).trim().to_string();

    // The reattach rides it. The ticker kept running while the session was
    // detached, so what comes back is whatever it has reached by now.
    let mut c = Client::spawn(std::path::Path::new(&h.client), &argv, &env);
    c.wait_until("the ticker is back", |c| numbers(&c.text()).len() > 5, T);
    let second = c.text();
    assert!(second.contains("shared ssh master at"), "{second}");
    assert!(!second.contains("did not answer"), "{second}");
    let again = control(&sock, "check");
    assert!(again.status.success());
    assert_eq!(
        String::from_utf8_lossy(&again.stderr).trim(),
        pid,
        "a second master was started instead of the first being used"
    );

    let cold = total_ms(&first, "first connection", "ACS-READY seen");
    let warm = total_ms(&second, "first connection", "ACS-READY seen");
    eprintln!("MEASURED handshake to ACS-READY: cold {cold:?} ms, on the master {warm:?} ms");
    assert!(cold.is_some() && warm.is_some(), "{first}\n---\n{second}");

    // The link dies: the master's own connection is the one that failed,
    // so the redial takes it down and dials its own.
    h.docker(&[
        "exec",
        &h.container,
        "sh",
        "-c",
        "pkill -f '[s]shd-session: dev@' || pkill -f '[s]shd: dev@'",
    ]);
    c.wait_resumed();
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 50), T);
    assert_consecutive(&c.text());
    let gone = control(&sock, "check");
    assert!(
        !gone.status.success(),
        "the master the lost link ran on is still up: {}",
        String::from_utf8_lossy(&gone.stderr)
    );

    c.send(&command(b'x'));
    c.wait(T);
    // Nothing of ours is left holding a connection to the host.
    let _ = control(&sock, "exit");
    eprintln!(
        "VERIFIED shared ssh master over ssh: {pid} carried the reattach, \
         socket 0600 in a 0700 directory, ended with the lost link"
    );
}
