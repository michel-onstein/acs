//! The configuration file (acs-xfk, DESIGN §7.2).

mod common;

use acs::testutil::TempDir;
use common::*;

const CMD: &[&str] = &["--", "/bin/sh", "-c", "echo up; sleep 30"];

fn args(session: &str) -> Vec<&str> {
    let mut a = vec!["devbox", session];
    a.extend_from_slice(CMD);
    a
}

fn env_refs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[test]
fn install_on_remote_false_stops_a_remote_install() {
    let remote = Remote::new();
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "# no installs\ninstall_on_remote: false\n");
    let mut c = Client::start_env(&remote, &args("s"), &env_refs(&env));
    assert_eq!(c.wait(T), 254);
    c.wait_for(
        &format!("acs {} is not installed on devbox (", acs::VERSION),
        T,
    );
    c.wait_for("install_on_remote is false (", T);
    c.wait_for("acs/config.yaml:2)", T);
    c.wait_for(&format!("install.sh | ACS_VERSION={} sh`", acs::VERSION), T);
    assert!(!c.text().contains("installing acs"), "{}", c.text());
    assert!(!remote.installed_binary(acs::VERSION).exists());
}

#[test]
fn the_local_file_overrides_the_global_one() {
    let remote = Remote::new();
    let cfg = TempDir::new();
    let global = cfg.path().join("global.yaml");
    std::fs::write(&global, "install_on_remote: false\n").unwrap();
    let mut env = config_env(cfg.path(), "install_on_remote: true\n");
    env.push(("ACS_GLOBAL_CONFIG".into(), global.display().to_string()));
    let mut c = Client::start_env(&remote, &args("s"), &env_refs(&env));
    c.wait_for(&format!("installed acs {} on devbox", acs::VERSION), T);
    c.wait_for("up", T);

    // The global file alone refuses.
    let remote = Remote::new();
    let only_global = vec![
        ("ACS_GLOBAL_CONFIG", global.to_str().unwrap()),
        ("XDG_CONFIG_HOME", NO_CONFIG),
    ];
    let mut c = Client::start_env(&remote, &args("g"), &only_global);
    assert_eq!(c.wait(T), 254);
    c.wait_for("global.yaml:1)", T);
}

#[test]
fn a_malformed_file_is_an_error_naming_file_and_line() {
    let remote = Remote::installed();
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "aliases:\n  devbox:\n\t- host: x\n");
    let out = acs_cmd()
        .envs(env)
        .args(["list", "--transport-cmd", &remote.transport(), "devbox"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("acs/config.yaml:3: tabs are not allowed"),
        "{err}"
    );
    assert!(out.stdout.is_empty());
}

/// Regression (acs-qjx): a file whose keys are separated by a tab, behind a
/// BOM, is read — it used to be "expected settings as 'key: value' lines".
#[test]
fn a_tab_after_the_colon_and_a_bom_are_read() {
    let remote = Remote::installed();
    let cfg = TempDir::new();
    let env = config_env(
        cfg.path(),
        "\u{feff}install_on_remote:\tfalse\naliases:\n  devbox:\n    - host:\tdevbox.lan\n",
    );
    let out = acs_cmd()
        .envs(env)
        .args(["config", "show"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("install_on_remote: false"), "{shown}");
    assert!(shown.contains("devbox.lan"), "{shown}");
    let _ = remote;
}

#[test]
fn help_and_version_ignore_a_broken_file() {
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "nonsense: [\n");
    for a in ["--help", "--version"] {
        let out = acs_cmd().envs(env.clone()).arg(a).output().unwrap();
        assert!(out.status.success(), "{a}");
    }
}

// ---- local_forwards (acs-odd) ----------------------------------------------

/// The alias `devbox` with a standing forward, and `nas` beside it, so a
/// listing over every alias has two hosts to ask at once.
const FORWARDED: &str = "\
local_forwards: [45997:localhost:9]
aliases:
  devbox:
    - host: devbox.lan
  nas:
    - host: nas.lan
";

/// acs-odd: a forward that comes from the configuration reaches the
/// **session's** ssh and no other call — the same `Call::Session` gate
/// acs-6f5 put the command line's `-L` behind (DESIGN §7.1). Several ssh
/// processes each binding one local port is what that gate prevents, and a
/// standing forward makes them likelier than a typed one ever did.
#[test]
fn a_configured_forward_goes_to_the_session_ssh_and_no_other_call() {
    let remote = Remote::installed();
    let ssh = Ssh::new(&[("devbox.lan", &remote), ("nas.lan", &remote)]);
    let net = Net::new(&["devbox.lan", "nas.lan"]);
    let env = net.env(FORWARDED);
    let ssh_path = ssh.path().display().to_string();

    // Call::Session — the forward is on it, out of the file, with no -L
    // anywhere on the command line.
    let mut a = vec!["--ssh".to_string(), ssh_path.clone(), "-v".into()];
    a.extend(["devbox", "s", "--", "/bin/sh", "-c", "echo up; sleep 30"].map(String::from));
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut c = Client::spawn(&exe(), &a, &refs(&env));
    c.wait_for("devbox: local_forwards 45997:localhost:9 (", T);
    c.wait_for("acs/config.yaml:2)", T);
    c.wait_session("s");
    c.wait_for("up", T);
    let session = ssh.calls();
    assert_eq!(session.len(), 1, "{session:?}");
    assert!(
        SshCall::of(&session[0])
            .opts
            .contains("-L 45997:localhost:9"),
        "{session:?}"
    );
    c.send(&command(b'x'));
    c.wait(T);

    // Call::Side — `acs list <host>` is an ssh process of its own, and
    // binding the port there would collide with the session holding it.
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", &ssh_path, "devbox"])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );
    assert!(out.status.success(), "{out:?}");
    let all = ssh.calls();
    let side = &all[session.len()..];
    assert_eq!(side.len(), 1, "{side:?}");
    assert!(!SshCall::of(&side[0]).opts.contains("-L"), "{side:?}");
    assert!(!side[0].contains("45997"), "{side:?}");

    // Call::Batch — `acs list` asks every alias at once, so the same
    // forward on each would have them fighting over one port.
    let before = ssh.calls().len();
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", &ssh_path])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );
    assert!(out.status.success(), "{out:?}");
    let all = ssh.calls();
    let batch = &all[before..];
    assert_eq!(batch.len(), 2, "{batch:?}");
    for call in batch {
        assert!(call.contains("-o BatchMode=yes"), "{call:?}");
        assert!(!SshCall::of(call).opts.contains("-L"), "{call:?}");
        assert!(!call.contains("45997"), "{call:?}");
    }
}

/// acs-odd: the alias's own list beats the global one, and `none` on an
/// alias means it forwards nothing however the global setting reads.
#[test]
fn an_aliass_own_forwards_replace_the_global_ones() {
    let remote = Remote::installed();
    let ssh = Ssh::new(&[("db.lan", &remote), ("quiet.lan", &remote)]);
    let net = Net::new(&["db.lan", "quiet.lan"]);
    let env = net.env(
        "local_forwards: [45997:localhost:9]\n\
         aliases:\n  \
           db:\n    local_forwards: 45998:db.internal:5432\n    hosts:\n      - host: db.lan\n  \
           quiet:\n    local_forwards: none\n    hosts:\n      - host: quiet.lan\n",
    );
    let ssh_path = ssh.path().display().to_string();
    let session = |alias: &str, name: &str| {
        let mut a = vec!["--ssh".to_string(), ssh_path.clone()];
        a.extend([alias, name, "--", "/bin/sh", "-c", "echo up; sleep 30"].map(String::from));
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let mut c = Client::spawn(&exe(), &a, &refs(&env));
        c.wait_session(name);
        c.wait_for("up", T);
        c.send(&command(b'x'));
        c.wait(T);
    };
    session("db", "a");
    let call = ssh.calls().pop().unwrap();
    assert!(
        SshCall::of(&call)
            .opts
            .contains("-L 45998:db.internal:5432"),
        "{call:?}"
    );
    assert!(!call.contains("45997"), "{call:?}");

    session("quiet", "b");
    let call = ssh.calls().pop().unwrap();
    assert!(!SshCall::of(&call).opts.contains("-L"), "{call:?}");
}

/// acs-odd: `-L none` drops the configured forwards for one run, and any
/// other `-L` beside it still applies. Without it the two add up.
#[test]
fn dash_l_none_drops_the_configured_forwards_and_keeps_the_rest() {
    let remote = Remote::installed();
    let ssh = Ssh::new(&[("devbox.lan", &remote)]);
    let net = Net::new(&["devbox.lan"]);
    let env = net.env(FORWARDED);
    let ssh_path = ssh.path().display().to_string();
    let session = |extra: &[&str], name: &str| {
        let mut a = vec!["--ssh".to_string(), ssh_path.clone()];
        a.extend(extra.iter().map(|s| s.to_string()));
        a.extend(["devbox", name, "--", "/bin/sh", "-c", "echo up; sleep 30"].map(String::from));
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let mut c = Client::spawn(&exe(), &a, &refs(&env));
        c.wait_session(name);
        c.wait_for("up", T);
        c.send(&command(b'x'));
        c.wait(T);
    };
    session(&["-L", "none"], "a");
    let call = ssh.calls().pop().unwrap();
    assert!(!SshCall::of(&call).opts.contains("-L"), "{call:?}");

    session(&["-L", "none", "-L", "45996:localhost:9"], "b");
    let call = ssh.calls().pop().unwrap();
    assert!(
        SshCall::of(&call).opts.contains("-L 45996:localhost:9"),
        "{call:?}"
    );
    assert!(!call.contains("45997"), "{call:?}");

    session(&["-L", "45996:localhost:9"], "c");
    let call = ssh.calls().pop().unwrap();
    assert!(
        SshCall::of(&call).opts.contains("-L 45996:localhost:9"),
        "{call:?}"
    );
    assert!(
        SshCall::of(&call).opts.contains("-L 45997:localhost:9"),
        "{call:?}"
    );
}

/// acs-odd: a bad spec in the file is a configuration error naming the
/// line, found when the file is read — not an ssh complaint across the
/// terminal mid-reconnect. `-L` is checked by the very same checker.
#[test]
fn a_bad_configured_forward_is_refused_when_the_file_is_read() {
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "local_forwards: [8080:localhost]\n");
    let out = acs_cmd()
        .envs(env)
        .args(["list", "devbox"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("acs/config.yaml:1: local_forwards: bad -L '8080:localhost'"),
        "{err}"
    );
    assert!(err.contains("port:host:hostport"), "{err}");
    assert!(out.stdout.is_empty());
}
