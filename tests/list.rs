//! `acs list <host>` (acs-5v9.12), and `acs list` on every alias
//! (acs-7zi, DESIGN §7.3).

mod common;

use std::time::{Duration, Instant};

use common::*;

fn list(remote: &Remote) -> (i32, String) {
    let out = acs_cmd()
        .args(["list", "--transport-cmd", &remote.transport(), "devbox"])
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn lists_attached_detached_and_drops_stale_sockets() {
    let remote = Remote::installed();
    let mut a = Client::start(
        &remote,
        &["devbox", "main", "--", "/bin/sh", "-c", "echo a; sleep 30"],
    );
    a.wait_for("a", T);
    let mut b = Client::start(
        &remote,
        &["devbox", "work", "--", "/bin/sh", "-c", "echo b; sleep 30"],
    );
    b.wait_for("b", T);
    b.send(&command(b'd'));
    assert_eq!(b.wait(T), 0);
    let stale = remote.sockets().join("old.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());

    let (code, out) = list(&remote);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("NAME"), "{out}");
    let row = |name: &str| {
        lines
            .iter()
            .find(|l| l.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no row for {name}:\n{out}"))
            .split_whitespace()
            .collect::<Vec<_>>()
    };
    assert_eq!(row("main")[1..3], ["attached", "tester@local"]);
    assert_eq!(row("work")[1..3], ["detached", "(tester@local)"]);
    assert!(row("work")
        .join(" ")
        .ends_with("/bin/sh -c echo b; sleep 30"));
    assert!(!out.contains("old"), "stale session listed:\n{out}");
    assert!(!stale.exists(), "stale socket not removed");
}

#[test]
fn empty_and_not_installed() {
    let remote = Remote::installed();
    assert_eq!(list(&remote), (0, "no sessions on devbox\n".into()));
    let bare = Remote::new();
    let (code, out) = list(&bare);
    assert_eq!(code, 0);
    assert!(out.contains("is not installed there"), "{out}");
}

/// Regression: a host that starts acs and then says nothing is given up on
/// in time; listing used to wait for it forever.
#[test]
fn a_host_silent_after_its_marker_is_given_up_on() {
    let remote = Remote::installed();
    remote.silence(Some(&acs::proto::ready_line()));
    let t0 = Instant::now();
    let out = acs_cmd()
        .env("ACS_DIAL_TIMEOUT_MS", "500")
        .args(["list", "--transport-cmd", &remote.transport(), "devbox"])
        .output()
        .unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    assert_eq!(out.status.code(), Some(255));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no answer from devbox within 0.5 s"), "{err}");
}

// ---- acs list: every alias --------------------------------------------------

/// `acs list [args]` with no host, through `ssh` and `net`: exit code,
/// stdout, stderr.
fn list_all(
    ssh: &Ssh,
    net: &Net,
    config: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> (i32, String, String) {
    let out = output_of(
        acs_cmd()
            .envs(net.env(config))
            .envs(env.iter().copied())
            .arg("list")
            .arg("--ssh")
            .arg(ssh.path())
            .args(args),
    );
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const EVERY_KIND: &str = "\
aliases:
  devbox:
    - host: devbox.lan
    - host: devbox.example.com
      user: me
  nas:
    - host: nas.lan
  pi:
    - host: pi.lan
  old:
    - host: old.lan
  lab:
    - host: lab.lan
      reachability_check: false
";

#[test]
fn every_alias_is_listed_and_a_dead_one_gets_a_line() {
    // devbox falls back to its second host, which has two sessions; nas has
    // none; pi lacks acs; old answers no ping; lab refuses ssh.
    let devbox = Remote::installed();
    let nas = Remote::installed();
    let pi = Remote::new();
    let mut a = Client::start(
        &devbox,
        &["devbox", "main", "--", "/bin/sh", "-c", "echo a; sleep 30"],
    );
    a.wait_for("a", T);
    let mut b = Client::start(
        &devbox,
        &["devbox", "work", "--", "/bin/sh", "-c", "echo b; sleep 30"],
    );
    b.wait_for("b", T);
    b.send(&command(b'd'));
    assert_eq!(b.wait(T), 0);

    let ssh = Ssh::new(&[
        ("me@devbox.example.com", &devbox),
        ("nas.lan", &nas),
        ("pi.lan", &pi),
    ]);
    let net = Net::new(&["devbox.example.com", "nas.lan", "pi.lan"]);
    let (code, out, err) = list_all(&ssh, &net, EVERY_KIND, &["-p", "2222"], &[]);
    assert_eq!(code, 255, "{out}{err}");

    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["HOST", "NAME", "STATE", "WHO", "IDLE", "AGE", "COMMAND"],
        "{out}"
    );
    let row = |name: &str| {
        lines
            .iter()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .find(|w| w.get(1) == Some(&name))
            .unwrap_or_else(|| panic!("no row for {name}:\n{out}"))
    };
    assert_eq!(
        row("main")[..4],
        ["devbox", "main", "attached", "tester@local"]
    );
    assert_eq!(
        row("work")[..4],
        ["devbox", "work", "detached", "(tester@local)"]
    );
    assert_eq!(
        lines[3..],
        [
            "no sessions on nas".to_string(),
            format!(
                "no sessions on pi (acs {} is not installed there)",
                acs::VERSION
            ),
        ],
        "{out}"
    );

    assert!(
        err.contains("acs: old: no host for 'old' is reachable (tried old.lan)"),
        "{err}"
    );
    assert!(
        err.contains("acs: lab: the connection closed before acs started on the remote"),
        "{err}"
    );

    // Each host was asked once, with nothing to prompt for and with the
    // user's options; the one no ping reached was never dialled.
    let calls = ssh.calls();
    let mut dests: Vec<&str> = calls
        .iter()
        .map(|c| c.split(" -- ").nth(1).unwrap().split(' ').next().unwrap())
        .collect();
    dests.sort();
    assert_eq!(
        dests,
        ["lab.lan", "me@devbox.example.com", "nas.lan", "pi.lan"]
    );
    for c in &calls {
        assert!(c.starts_with("-T -e none -o BatchMode=yes"), "{c}");
        assert!(c.contains(" -p 2222 -- "), "{c}");
    }
}

#[test]
fn every_host_answering_exits_0() {
    let nas = Remote::installed();
    let pi = Remote::new();
    let ssh = Ssh::new(&[("nas.lan", &nas), ("pi.lan", &pi)]);
    let net = Net::new(&["nas.lan", "pi.lan"]);
    let config = "aliases:\n  nas:\n    - host: nas.lan\n  pi:\n    - host: pi.lan\n";
    let (code, out, err) = list_all(&ssh, &net, config, &[], &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "no sessions on nas\nno sessions on pi (acs {} is not installed there)\n",
            acs::VERSION
        )
    );
    assert_eq!(err, "");
}

#[test]
fn every_host_is_asked_with_its_own_key() {
    // nas has its own key, pi takes its alias's, and -i on the command
    // line would replace both (acs-mbd).
    let nas = Remote::installed();
    let pi = Remote::installed();
    let ssh = Ssh::new(&[("nas.lan", &nas), ("pi.lan", &pi)]);
    let net = Net::new(&["nas.lan", "pi.lan"]);
    let config = "\
aliases:
  nas:
    - host: nas.lan
      identity_file: /keys/nas
  pi:
    identity_file: /keys/pi
    hosts:
      - host: pi.lan
";
    let (code, _, err) = list_all(&ssh, &net, config, &[], &[]);
    assert_eq!(code, 0, "{err}");
    let mut keys = ssh.keys();
    keys.sort();
    assert_eq!(
        keys,
        [
            ("nas.lan".to_string(), vec!["/keys/nas".to_string()]),
            ("pi.lan".to_string(), vec!["/keys/pi".to_string()])
        ]
    );
    let (code, _, err) = list_all(&ssh, &net, config, &["-i", "/cli/key"], &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        ssh.keys()[2..].iter().all(|(_, k)| k == &["/cli/key"]),
        "{:?}",
        ssh.keys()
    );
}

#[test]
fn silent_hosts_are_waited_for_in_parallel() {
    // Two say nothing at all, one starts acs and then says nothing.
    let quiet: Vec<Remote> = (0..3).map(|_| Remote::installed()).collect();
    quiet[0].silence(Some(""));
    quiet[1].silence(Some(""));
    quiet[2].silence(Some(&acs::proto::ready_line()));
    let nas = Remote::installed();
    let ssh = Ssh::new(&[
        ("q0", &quiet[0]),
        ("q1", &quiet[1]),
        ("q2", &quiet[2]),
        ("nas.lan", &nas),
    ]);
    let net = Net::new(&["q0", "q1", "q2", "nas.lan"]);
    let config = "\
aliases:
  q0:
    - host: q0
  q1:
    - host: q1
  q2:
    - host: q2
  nas:
    - host: nas.lan
";
    // Long enough for nas to answer on a loaded test machine.
    let limit = [("ACS_DIAL_TIMEOUT_MS", "6000")];
    let t0 = Instant::now();
    let (code, out, err) = list_all(&ssh, &net, config, &[], &limit);
    // One after another would take 18 s.
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(code, 255);
    assert_eq!(out, "no sessions on nas\n", "{err}");
    for q in ["q0", "q1", "q2"] {
        assert!(
            err.contains(&format!("acs: {q}: no answer from {q} within 6 s")),
            "{err}"
        );
    }
}

#[test]
fn no_aliases_is_a_usage_error() {
    let ssh = Ssh::new(&[]);
    let net = Net::new(&[]);
    let (code, out, err) = list_all(&ssh, &net, "update_check: false\n", &[], &[]);
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert!(
        err.contains("acs: no host aliases in the configuration"),
        "{err}"
    );
    assert!(ssh.calls().is_empty());
}

/// acs-pcs: with no configuration file at all `acs list` says so, naming
/// where it looked, rather than blaming a configuration that is not there;
/// a file without aliases keeps its own message. Both exit 2, from the
/// table and from the menu alike.
#[test]
fn no_configuration_at_all_is_told_apart_from_one_without_aliases() {
    let hint = "list one host with acs list <host>, or add an alias with acs config host add <alias> <host>";
    let none = format!(
        "acs: no configuration (looked for {NO_CONFIG}/global.yaml and {NO_CONFIG}/acs/config.yaml): {hint}\n"
    );
    // The harness points both files at a directory that does not exist.
    let out = acs_cmd().arg("list").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&out.stderr), none);
    let mut c = Client::spawn(&exe(), &["list"], &[]);
    assert_eq!(c.wait(T), 2);
    c.wait_for(none.trim_end(), T);
    // A file, even an empty one or one with an empty aliases:, is a
    // configuration without aliases.
    for yaml in ["", "aliases:\n", "update_check: false\n"] {
        let cfg = acs::testutil::TempDir::new();
        let env = config_env(cfg.path(), yaml);
        let out = acs_cmd().envs(env).arg("list").output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{yaml:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("acs: no host aliases in the configuration: {hint}\n"),
            "{yaml:?}"
        );
    }
}
