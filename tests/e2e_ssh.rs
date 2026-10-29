//! End-to-end over real ssh against a container host (acs-5v9.24). Run by
//! `scripts/e2e_ssh.sh`, which starts the host, sets `ACS_E2E_*` and passes
//! `--include-ignored`. Every test is `#[ignore]`, so a plain `cargo test`
//! counts them as ignored rather than passing them unrun (acs-hdj4), and one
//! asked for without the host fails instead of returning early.
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

/// The host `scripts/e2e_ssh.sh` started. A test only gets here when it was
/// asked for by name or with `--include-ignored`, so a missing variable is a
/// failure, not a reason to pass without running.
fn host() -> Host {
    let v = |k: &str| {
        std::env::var(k)
            .unwrap_or_else(|_| panic!("{k} is not set: this test runs under scripts/e2e_ssh.sh"))
    };
    Host {
        client: v("ACS_E2E_CLIENT"),
        key: v("ACS_E2E_KEY"),
        port: v("ACS_E2E_PORT"),
        container: v("ACS_E2E_CONTAINER"),
    }
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
        self.client_as("dev", session, cmd, env)
    }

    fn client_as(&self, user: &str, session: &str, cmd: &str, env: &[(&str, &str)]) -> Client {
        let mut args = self.ssh_args();
        args.push(format!("{user}@127.0.0.1"));
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

    /// `sh -c script` in the container, as root; its stdout.
    fn root_sh(&self, script: &str) -> String {
        let out = Command::new("docker")
            .args(["exec", &self.container, "sh", "-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "in the container: {script}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

const TICKER: &str = "i=0; while true; do i=$((i+1)); printf '#%d#\\n' $i; sleep 0.02; done";

// `numbers` and `assert_consecutive` live in tests/common, with
// tests/reconnect.rs, which reads the same ticker (acs-2dc).

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
    poll(
        timeout,
        || greeting(port),
        || format!("nothing answered on 127.0.0.1:{port} within {timeout:?}"),
    )
}

#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_01_first_contact_installs_and_attaches() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_02_identity_file_is_required() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_03_vim_uses_the_alternate_screen_and_exits_cleanly() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_04_osc52_and_hyperlinks_pass_through_verbatim() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_05_kitty_keyboard_protocol_command_key() {
    let h = host();
    let mut c = h.client("kitty", "printf '\\033[>1u'; echo kitty-on; sleep 60", &[]);
    c.wait_for("kitty-on", T);
    c.send(b"\x1b[93;5u\x1b[93;5ud");
    assert_eq!(c.wait(T), 0);
    c.wait_for("\x1b[<1u", T);
    eprintln!("VERIFIED Ctrl-] Ctrl-] d in kitty encoding detaches; kitty flags popped");
}

#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_06_mouse_reports_reach_the_program() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_07_killed_connection_resumes_without_loss() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_08_frozen_host_is_detected_and_resumed() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_09_list_shows_the_sessions() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_09b_list_without_a_host_asks_every_alias() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_10_user_at_alias_logs_in_as_that_user() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_11_identity_file_from_the_configuration() {
    let h = host();
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_12_ctrl_l_after_a_reattach_and_a_resume() {
    let h = host();
    let mut c = h.client("redraw", REPORTER, &[]);
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_13_plain_host_picks_a_detached_session_from_the_menu() {
    let h = host();
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
    // End menu-b: the cursor down to it, x, y.
    let b = menu_row(&c, "menu-b");
    c.send(&vec![b'j'; b - 1]);
    c.send(b"x");
    c.wait_for("end session 'menu-b'?", T);
    c.send(b"y");
    c.wait_for("session 'menu-b' ended", T);
    // Attach menu-a by its number.
    let a = menu_row(&c, "menu-a");
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
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_14_a_local_forward_is_the_session_connections_alone() {
    let h = host();
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

/// The total of `what`'s first `phase` timing line in a client's output.
fn total_ms(text: &str, what: &str, phase: acs::timing::Phase) -> Option<u64> {
    acs::timing::told(text)
        .into_iter()
        .find(|t| t.what == what && t.phase == phase.name())
        .map(|t| t.total_ms)
}

/// acs-9n3, over real ssh — the one thing a fake transport cannot test.
///
/// Three claims, in order: a detached session leaves a master behind and
/// the next `acs` rides it rather than handshaking again; the socket it
/// rides is a `0600` file in a `0700` directory of this user's; and a lost
/// link takes the master down with it, because the master's own connection
/// is the one that failed.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_15_a_shared_master_carries_the_second_connection_and_dies_with_the_link() {
    use std::os::unix::fs::PermissionsExt;
    let h = host();
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

    let cold = total_ms(&first, "first connection", acs::timing::Phase::ReadySeen);
    let warm = total_ms(&second, "first connection", acs::timing::Phase::ReadySeen);
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

/// The system-wide install alone (acs-1gy): `solo` has nothing under its
/// `$HOME`, and root has put this version at `/usr/local/lib/acs/<v>/acs`,
/// as an administrator following DESIGN §8 would. The dial must attach on
/// root's copy with no first-contact install and leave no second copy
/// behind. The binary is the Linux build from this same `dist/`, so the test
/// does not lean on `e2e_01` having installed one for `dev`.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_16_the_system_wide_install_alone_attaches_without_installing() {
    let h = host();
    let v = acs::VERSION;
    let arch = h.root_sh("uname -m").trim().to_string();
    let linux = std::path::Path::new(&h.client)
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join(format!("{arch}-unknown-linux-musl/acs"));
    assert!(linux.is_file(), "no Linux build for the host at {linux:?}");
    let system = format!("/usr/local/lib/acs/{v}");
    h.root_sh(&format!("mkdir -p {system}"));
    h.docker(&[
        "cp",
        linux.to_str().unwrap(),
        &format!("{}:{system}/acs", h.container),
    ]);
    // Root's, and writable by nobody else: what the prelude accepts.
    h.root_sh(&format!(
        "chown -R root:root /usr/local/lib/acs \
         && chmod 755 /usr/local/lib/acs {system} {system}/acs"
    ));
    let own = "ls -A /home/solo/.local/share/acs /home/solo/.local/bin 2>/dev/null; true";
    assert_eq!(
        h.root_sh(own).trim(),
        "",
        "solo starts with an acs of its own"
    );

    let mut c = h.client_as("solo", "sys", "echo \"up-as-$(id -un)\"; sleep 60", &[]);
    c.wait_for("up-as-solo", T);
    // The prelude `exec`s the binary it picked as the proxy, so the path it
    // picked is in solo's process list.
    let ps = h.root_sh("ps -o user,args");
    assert!(
        ps.lines()
            .any(|l| l.trim_start().starts_with("solo")
                && l.contains(&format!("{system}/acs _proxy"))),
        "no process of solo's runs {system}/acs:\n{ps}"
    );
    c.send(&command(b'x'));
    c.wait(T);
    let text = c.text();
    assert!(!text.contains("installing acs"), "{text}");
    assert!(!text.contains("installed acs"), "{text}");
    // No second copy and no `~/.local/bin` link; and the proxy's version
    // housekeeping, which only ever touches the user's own versions
    // directory, made none either.
    assert_eq!(h.root_sh(own).trim(), "", "solo got an acs of its own");
    // Gone again, so that every user after this one meets the host as it
    // was: with it there, e2e_18's first contact would find root's copy.
    h.root_sh(&format!("rm -rf {system}"));
    eprintln!("VERIFIED the system-wide install alone attaches over ssh, installing nothing");
}

/// acs-hmv, over real ssh: `acs list` over every alias joins the master a
/// session has up — `ControlMaster=no` on acs's socket — and starts none;
/// the session on that master keeps streaming through the listings; and
/// the time a listing takes is measured on the master and off it.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_17_a_listing_over_every_alias_joins_the_master_a_session_has_up() {
    let h = host();
    let dir = acs::testutil::TempDir::new();
    let mux = dir.path().join("mux");
    let mux_s = mux.to_str().unwrap().to_string();
    let mut env = config_env(
        dir.path(),
        "aliases:\n  box:\n    - host: 127.0.0.1\n      user: dev\n",
    );
    env.extend([
        ("ACS_CONTROL_DIR".to_string(), mux_s.clone()),
        ("ACS_CONTROL_PERSIST".to_string(), "300".to_string()),
        ("ACS_NO_UPDATE_CHECK".to_string(), "1".to_string()),
        (
            "ACS_GLOBAL_CONFIG".to_string(),
            format!("{NO_CONFIG}/global.yaml"),
        ),
    ]);
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let check = |sock: &std::path::Path| -> Option<String> {
        let mut args = vec!["-o".to_string(), format!("ControlPath={}", sock.display())];
        args.extend(h.ssh_args());
        args.extend(["-O", "check", "--", "dev@127.0.0.1"].map(String::from));
        let out = Command::new("ssh").args(&args).output().unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
    };
    // One listing; how long it took, and what it said under -v.
    let list = |persist: &str| -> (u128, String, String) {
        let mut args = vec!["list".to_string(), "-v".into()];
        args.extend(h.ssh_args());
        let start = Instant::now();
        let out = Command::new(&h.client)
            .args(&args)
            .envs(env_refs.iter().copied())
            .env("ACS_CONTROL_PERSIST", persist)
            .output()
            .unwrap();
        let ms = start.elapsed().as_millis();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{text}{err}");
        (ms, text, err)
    };

    // No master yet: the listing starts none.
    let (_, _, err) = list("300");
    assert!(!err.contains("ControlPath="), "{err}");
    assert!(
        std::fs::read_dir(&mux).map_or(0, |d| d.count()) == 0,
        "a listing left a socket"
    );

    // A session, attached and streaming, which starts the master.
    let mut c = h.client("hmv", TICKER, &env_refs);
    c.wait_for("#10#", T);
    let sock = one_socket(&mux);
    let pid = check(&sock).expect("no master under the session");

    let mut on = Vec::new();
    let mut off = Vec::new();
    for _ in 0..5 {
        let (ms, text, err) = list("300");
        assert!(
            text.lines()
                .any(|l| l.starts_with("box ") && l.contains(" hmv ")),
            "{text}{err}"
        );
        assert!(err.contains("ControlMaster=no"), "{err}");
        assert!(
            err.contains(&format!("ControlPath={}", sock.display())),
            "{err}"
        );
        assert!(!err.contains("ControlMaster=auto"), "{err}");
        assert!(!err.contains("did not answer"), "{err}");
        on.push(ms);
        // The same listing on a connection of its own.
        let (ms, _, err) = list("0");
        assert!(!err.contains(&sock.display().to_string()), "{err}");
        off.push(ms);
    }
    on.sort();
    off.sort();
    eprintln!(
        "MEASURED acs list over every alias: on the master {on:?} ms, own connection {off:?} ms (median {} vs {})",
        on[2], off[2]
    );

    // The master is the one the session started, and still the only one;
    // the session streamed straight through every listing.
    assert_eq!(check(&sock).as_deref(), Some(&pid[..]));
    assert_eq!(one_socket(&mux), sock);
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 20), T);
    assert_consecutive(&c.text());
    assert!(!c.text().contains("connection lost"), "{}", c.text());

    c.send(&command(b'x'));
    c.wait(T);
    let mut args = vec!["-o".to_string(), format!("ControlPath={}", sock.display())];
    args.extend(h.ssh_args());
    args.extend(["-O", "exit", "--", "dev@127.0.0.1"].map(String::from));
    let _ = Command::new("ssh").args(&args).output();
    eprintln!(
        "VERIFIED acs list over every alias joins the session's master ({pid}) and starts none"
    );
}

// ---- acscp (acs-hofu.8, ACSCP_DESIGN §11) ---------------------------------

impl Host {
    /// The ssh options again, spelled for `acs cp`: the port is `-P`, as
    /// for `scp`, since `-p` is preserve.
    fn cp_args(&self) -> Vec<String> {
        self.ssh_args()
            .into_iter()
            .map(|a| if a == "-p" { "-P".into() } else { a })
            .collect()
    }

    /// `acs cp <args>` from `cwd`, with no configuration of the caller's and
    /// no ssh master unless `env` asks for one; its exit code and stderr.
    fn cp(&self, cwd: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        let out = self.cp_command(cwd, args, env).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn cp_command(&self, cwd: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(&self.client);
        c.current_dir(cwd)
            .arg("cp")
            .args(self.cp_args())
            .args(args)
            .env("XDG_CONFIG_HOME", NO_CONFIG)
            .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
            .env("ACS_NO_UPDATE_CHECK", "1")
            .env("ACS_CONTROL_PERSIST", NO_MASTER)
            .envs(env.iter().copied());
        c
    }

    /// The host's clock, in Unix seconds: what its file times are read
    /// against.
    fn now(&self) -> u64 {
        self.root_sh("date +%s").trim().parse().unwrap()
    }

    /// How many logins sshd has accepted so far: each is a handshake.
    fn logins(&self) -> usize {
        let out = Command::new("docker")
            .args(["logs", &self.container])
            .output()
            .unwrap();
        let all = [out.stdout, out.stderr].concat();
        String::from_utf8_lossy(&all)
            .matches("Accepted publickey for ")
            .count()
    }
}

/// `n` bytes that are not all one value, so a chunk out of place shows.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

/// Every regular file under `root` as `relative path → (mode, mtime)`.
fn modes_and_times(root: &std::path::Path) -> Vec<(String, u32, i64)> {
    use std::os::unix::fs::MetadataExt;
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u32, i64)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let m = std::fs::metadata(&p).unwrap();
            if m.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, m.mode() & 0o7777, m.mtime()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The copy's own first contact, on an account nothing has reached before:
/// `acs cp` installs acs on the one connection and copies; the install
/// leaves both of the copy's names, `acscp` and `acs cp`, working on the
/// host; and `acs _cp` marks its version directory used, so a host reached
/// only by copies does not have that version pruned under it.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_18_acscp_first_contact_installs_acs_and_both_copy_names() {
    let h = host();
    let v = acs::VERSION;
    let home = "/home/copier";
    assert_eq!(
        h.root_sh(&format!("ls -A {home}/.local 2>/dev/null; true"))
            .trim(),
        "",
        "copier starts with an acs"
    );
    assert_eq!(
        h.root_sh(&format!("ls /usr/local/lib/acs/{v} 2>/dev/null; true"))
            .trim(),
        "",
        "a system-wide install would answer copier's first contact"
    );
    let dir = acs::testutil::TempDir::new();
    std::fs::write(dir.path().join("hello.txt"), "copied over ssh\n").unwrap();
    let (code, err) = h.cp(
        dir.path(),
        &["-v", "-q", "hello.txt", "copier@127.0.0.1:"],
        &[],
    );
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("installing acs"), "{err}");
    assert!(err.contains(&format!("installed acs {v}")), "{err}");
    assert_eq!(
        h.root_sh(&format!("cat {home}/hello.txt")),
        "copied over ssh\n"
    );

    // Both names: `acscp` a relative link to the `acs` link beside it.
    let links = h.root_sh(&format!(
        "readlink {home}/.local/bin/acscp; readlink {home}/.local/bin/acs"
    ));
    let links: Vec<&str> = links.lines().collect();
    assert_eq!(links[0], "acs", "{links:?}");
    assert!(links[1].contains(&format!("{v}/acs")), "{links:?}");
    let both = h.root_sh(&format!(
        "su copier -s /bin/sh -c 'cd && {home}/.local/bin/acscp hello.txt a.txt \
         && {home}/.local/bin/acs cp a.txt b.txt && cat b.txt'"
    ));
    assert_eq!(both, "copied over ssh\n", "acscp then acs cp on the host");

    // A copy marks its version used: set a year back, a copy brings it to now.
    let vdir = format!("{home}/.local/share/acs/{v}");
    h.root_sh(&format!("touch -t 200001010000 {vdir}"));
    let (code, err) = h.cp(dir.path(), &["-q", "copier@127.0.0.1:b.txt", "."], &[]);
    assert_eq!(code, 0, "{err}");
    let touched: u64 = h
        .root_sh(&format!("stat -c %Y {vdir}"))
        .trim()
        .parse()
        .unwrap();
    assert!(
        h.now().saturating_sub(touched) < 600,
        "acs _cp left its version directory at {touched}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "copied over ssh\n"
    );
    eprintln!(
        "VERIFIED acscp first contact: installed acs {v} on the copy's connection, \
         acscp and acs cp both work on the host, acs _cp touched {vdir}"
    );
}

/// A tree up and back down with `-r -p`: the same files (`diff -r`), and
/// each file's mode and modification time kept both ways.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_19_acscp_r_p_round_trip_is_identical() {
    let h = host();
    let dir = acs::testutil::TempDir::new();
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("sub/deeper")).unwrap();
    let files: [(&str, Vec<u8>, u32, u64); 5] = [
        ("a.txt", b"alpha\n".to_vec(), 0o640, 978_307_200),
        (
            "sub/run.sh",
            b"#!/bin/sh\necho hi\n".to_vec(),
            0o755,
            1_000_000_000,
        ),
        ("sub/deeper/empty", Vec::new(), 0o600, 1_100_000_000),
        ("sub/deeper/blob", noise(3 << 20, 7), 0o644, 1_200_000_000),
        (
            "sub/odd name [1]",
            b"spaces and brackets\n".to_vec(),
            0o604,
            1_300_000_000,
        ),
    ];
    use std::os::unix::fs::PermissionsExt;
    for (name, body, mode, t) in &files {
        let p = tree.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(*mode)).unwrap();
        acs::sys::set_mtime(&p, *t).unwrap();
    }
    h.root_sh("rm -rf /home/dev/tree");

    let (code, err) = h.cp(
        dir.path(),
        &["-r", "-p", "-q", "tree", "dev@127.0.0.1:"],
        &[],
    );
    assert_eq!(code, 0, "up: {err}");
    // On the host, as sent.
    let there =
        h.root_sh("cd /home/dev/tree && find . -type f -exec stat -c '%a %Y %n' {} + | sort");
    for (name, _, mode, t) in &files {
        let row = format!("{mode:o} {t} ./{name}");
        assert!(there.lines().any(|l| l == row), "no `{row}` in:\n{there}");
    }

    let back = dir.path().join("back");
    std::fs::create_dir(&back).unwrap();
    let (code, err) = h.cp(
        dir.path(),
        &["-r", "-p", "-q", "dev@127.0.0.1:tree", "back"],
        &[],
    );
    assert_eq!(code, 0, "down: {err}");
    let diff = Command::new("diff")
        .arg("-r")
        .arg(&tree)
        .arg(back.join("tree"))
        .output()
        .unwrap();
    assert!(
        diff.status.success(),
        "diff -r:\n{}{}",
        String::from_utf8_lossy(&diff.stdout),
        String::from_utf8_lossy(&diff.stderr)
    );
    assert_eq!(modes_and_times(&tree), modes_and_times(&back.join("tree")));
    eprintln!("VERIFIED acscp -r -p up and down over ssh: diff -r clean, modes and times kept");
}

/// The link killed mid-file (`sshd-session` on the host, as e2e_07 does to
/// a session): the copy redials by itself and resumes from the part on the
/// host — at least the quarter that was there when the link died — rather
/// than sending the file again, and what lands is the file.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_20_acscp_killed_mid_copy_resumes_from_the_part() {
    let h = host();
    let size: u64 = std::env::var("ACS_E2E_CP_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
        << 20;
    let dir = acs::testutil::TempDir::new();
    let big = dir.path().join("big");
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&big).unwrap());
        let chunk = 1 << 20;
        for i in 0..size / chunk {
            f.write_all(&noise(chunk as usize, i + 1)).unwrap();
        }
    }
    let want = acs::sha256::hex(&acs::sha256::file(&big).unwrap());
    h.root_sh("rm -f /home/dev/big /home/dev/.big.acscp-part");

    // Watched from inside the container, where a poll costs nothing: the
    // link dies once the part holds a quarter of the file.
    let quarter = size / 4;
    let watch = format!(
        "f=/home/dev/.big.acscp-part; \
         while [ ! -e /home/dev/big ] && [ \"$(stat -c %s $f 2>/dev/null || echo 0)\" -lt {quarter} ]; \
         do sleep 0.01; done; \
         if [ -e /home/dev/big ]; then echo finished-first; exit 0; fi; \
         echo killed-at $(stat -c %s $f); \
         pkill -f '[s]shd-session: dev@' || pkill -f '[s]shd: dev@'"
    );
    let watcher = Command::new("docker")
        .args(["exec", &h.container, "sh", "-c", &watch])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    let (code, err) = h.cp(
        dir.path(),
        &["-v", "-q", "big", "dev@127.0.0.1:"],
        &[("ACS_BACKOFF_MS", "300")],
    );
    let took = t0.elapsed();
    let said = String::from_utf8_lossy(&watcher.wait_with_output().unwrap().stdout).into_owned();
    assert_eq!(code, 0, "{err}");
    assert!(
        said.starts_with("killed-at "),
        "the link was not killed: {said}"
    );
    assert!(err.contains("reconnecting"), "{err}");
    let resumed: u64 = err
        .lines()
        .find_map(|l| {
            l.split("resuming after ")
                .nth(1)?
                .split(' ')
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or_else(|| panic!("no resume:\n{err}"));
    assert!(
        resumed >= quarter && resumed < size,
        "resumed after {resumed} of {size} bytes; the part held {quarter}+ at the kill:\n{err}"
    );
    let got = h.root_sh("sha256sum /home/dev/big");
    assert!(got.starts_with(&want), "{got} vs {want}");
    assert_eq!(
        h.root_sh("ls -A /home/dev | grep acscp-part; true").trim(),
        "",
        "a part was left behind"
    );
    h.root_sh("rm -f /home/dev/big");
    eprintln!(
        "VERIFIED acscp over a killed link: {} MB, the link {}, resumed after {resumed} bytes, \
         sha256 equal, {took:?} in all",
        size >> 20,
        said.trim()
    );
}

/// A copy to a host with a session up rides the session's ssh master: no
/// new login on the host (sshd's own log says so), the same master pid
/// after, still the one socket — and the session streams straight through.
#[test]
#[ignore = "needs the container host: run by scripts/e2e_ssh.sh"]
fn e2e_21_acscp_joins_the_sessions_master() {
    let h = host();
    let dir = acs::testutil::TempDir::new();
    let mux = dir.path().join("mux");
    let mux_s = mux.to_str().unwrap().to_string();
    let env: Vec<(&str, &str)> = vec![
        ("ACS_CONTROL_DIR", &mux_s),
        ("ACS_CONTROL_PERSIST", "300"),
        ("ACS_NO_UPDATE_CHECK", "1"),
    ];
    let check = |sock: &std::path::Path| -> Option<String> {
        let mut args = vec!["-o".to_string(), format!("ControlPath={}", sock.display())];
        args.extend(h.ssh_args());
        args.extend(["-O", "check", "--", "dev@127.0.0.1"].map(String::from));
        let out = Command::new("ssh").args(&args).output().unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stderr).trim().to_string())
    };
    let mut c = h.client("cpmux", TICKER, &env);
    c.wait_for("#10#", T);
    let sock = one_socket(&mux);
    let pid = check(&sock).expect("no master under the session");

    std::fs::write(dir.path().join("up.txt"), noise(1 << 20, 3)).unwrap();
    let before = h.logins();
    let (code, err) = h.cp(dir.path(), &["-v", "-q", "up.txt", "dev@127.0.0.1:"], &env);
    assert_eq!(code, 0, "up: {err}");
    assert!(err.contains("shared ssh master at"), "{err}");
    assert!(
        err.contains(&format!("ControlPath={}", sock.display())),
        "{err}"
    );
    let down = dir.path().join("down");
    std::fs::create_dir(&down).unwrap();
    let (code, err) = h.cp(dir.path(), &["-q", "dev@127.0.0.1:up.txt", "down"], &env);
    assert_eq!(code, 0, "down: {err}");
    let after = h.logins();
    assert_eq!(
        after, before,
        "the copies logged in again: a second handshake"
    );
    assert_eq!(check(&sock).as_deref(), Some(&pid[..]), "another master");
    assert_eq!(one_socket(&mux), sock);
    assert_eq!(
        std::fs::read(dir.path().join("up.txt")).unwrap(),
        std::fs::read(down.join("up.txt")).unwrap()
    );
    let last = *numbers(&c.text()).last().unwrap();
    c.wait_for(&format!("#{}#", last + 20), T);
    assert_consecutive(&c.text());

    c.send(&command(b'x'));
    c.wait(T);
    let mut args = vec!["-o".to_string(), format!("ControlPath={}", sock.display())];
    args.extend(h.ssh_args());
    args.extend(["-O", "exit", "--", "dev@127.0.0.1"].map(String::from));
    let _ = Command::new("ssh").args(&args).output();
    h.root_sh("rm -f /home/dev/up.txt");
    eprintln!(
        "VERIFIED acscp up and down ride the session's master ({pid}): \
         {before} logins before, {after} after"
    );
}
