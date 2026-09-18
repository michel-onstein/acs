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
    let mut args = h.ssh_args();
    args.extend(["dev@127.0.0.1".into(), "--list".into()]);
    let out = Command::new(&h.client).args(&args).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("first"), "{text}");
    eprintln!("VERIFIED --list over ssh:\n{text}");
}

#[test]
fn e2e_09b_list_without_a_host_asks_every_alias() {
    let Some(h) = host() else { return };
    // `box` is the container; `gone` is a documentation address no ping
    // reaches.
    let dir = acs::testutil::TempDir::new();
    let env = config_env(
        dir.path(),
        "hosts:\n  box:\n    - host: 127.0.0.1\n      user: dev\n  gone:\n    - host: 192.0.2.1\n",
    );
    let mut args = h.ssh_args();
    args.push("--list".into());
    let out = Command::new(&h.client)
        .args(&args)
        .envs(env)
        .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
        .env("ACS_NO_UPDATE_CHECK", "1")
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
    eprintln!("VERIFIED acs --list over ssh (BatchMode):\n{text}{err}");
}

#[test]
fn e2e_10_user_at_alias_logs_in_as_that_user() {
    let Some(h) = host() else { return };
    // The entry's own user has no key on the host: only the override works.
    let cfg = acs::testutil::TempDir::new();
    let env = config_env(
        cfg.path(),
        "hosts:\n  box:\n    - host: 127.0.0.1\n      user: nobody\n      reachability_check: false\n",
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
