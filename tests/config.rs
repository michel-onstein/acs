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
    let env = config_env(cfg.path(), "hosts:\n  devbox:\n\t- host: x\n");
    let out = acs_cmd()
        .envs(env)
        .args(["--transport-cmd", &remote.transport(), "devbox", "--list"])
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

#[test]
fn help_and_version_ignore_a_broken_file() {
    let cfg = TempDir::new();
    let env = config_env(cfg.path(), "nonsense: [\n");
    for a in ["--help", "--version"] {
        let out = acs_cmd().envs(env.clone()).arg(a).output().unwrap();
        assert!(out.status.success(), "{a}");
    }
}
