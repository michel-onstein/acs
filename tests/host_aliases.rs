//! Host aliases (acs-ue2, DESIGN §7.3). A fake `ping` (`ACS_PING`, `Net` in
//! tests/common) answers for the hosts listed in a file, so no ICMP is
//! needed. The fake transport ignores the destination, so `-v` output shows
//! which one was chosen.

mod common;

use common::*;

const CONFIG: &str = "\
hosts:
  devbox:
    - host: devbox.lan
    - host: devbox.example.com
      user: me
  lab:
    - host: lab1
    - host: lab2
      reachability_check: false
";

fn session(host: &str) -> Vec<&str> {
    vec!["-v", host, "s", "--", "/bin/sh", "-c", "echo up; sleep 30"]
}

#[test]
fn an_alias_connects_to_its_first_reachable_host() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan", "devbox.example.com"]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("devbox"), &refs(&env));
    c.wait_for("devbox: devbox.lan answers ping, using devbox.lan", T);
    c.wait_for("up", T);
    assert_eq!(net.pinged(), ["devbox.lan"]);
}

#[test]
fn falls_back_to_the_next_host_and_its_user() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.example.com"]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("devbox"), &refs(&env));
    c.wait_for("devbox: devbox.lan does not answer ping", T);
    c.wait_for(
        "devbox: devbox.example.com answers ping, using me@devbox.example.com",
        T,
    );
    c.wait_for("up", T);
}

#[test]
fn an_unchecked_host_is_used_without_a_ping() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("lab"), &refs(&env));
    c.wait_for("lab: using lab2 (reachability_check is off, ", T);
    c.wait_for("up", T);
    assert_eq!(net.pinged(), ["lab1"]);
}

#[test]
fn no_reachable_host_is_an_error_with_exit_255() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &["devbox", "s"], &refs(&env));
    assert_eq!(c.wait(T), 255);
    c.wait_for(
        "acs: no host for 'devbox' is reachable (tried devbox.lan, devbox.example.com)",
        T,
    );
    assert!(remote.transport_pids().is_empty(), "no connection is made");
}

#[test]
fn user_at_alias_logs_in_as_that_user_on_every_host() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("you@devbox"), &refs(&env));
    c.wait_for("you@devbox: logging in as you, from the command line", T);
    c.wait_for(
        "you@devbox: devbox.lan answers ping, using you@devbox.lan",
        T,
    );
    c.wait_for("up", T);
    c.send(&command(b'x'));
    c.wait(T);

    // The fallback host's own `user: me` gives way to the given one.
    net.set_up(&["devbox.example.com"]);
    let mut c = Client::start_env(&remote, &session("you@devbox"), &refs(&env));
    c.wait_for(
        "you@devbox: devbox.example.com answers ping, using you@devbox.example.com",
        T,
    );
    c.wait_for("up", T);
    c.send(&command(b'x'));
    c.wait(T);
}

#[test]
fn other_names_are_not_resolved() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(CONFIG);
    for host in ["other", "me@other"] {
        let mut c = Client::start_env(&remote, &session(host), &refs(&env));
        c.wait_for("up", T);
        c.send(&command(b'x'));
        c.wait(T);
        assert!(!c.text().contains("ping"), "{host}: {}", c.text());
    }
    assert!(net.pinged().is_empty());
}

#[test]
fn list_resolves_the_alias_too() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let out = acs_cmd()
        .envs(net.env(CONFIG))
        .args(["--transport-cmd", &remote.transport(), "devbox", "--list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no host for 'devbox'"));

    net.set_up(&["devbox.lan"]);
    let out = acs_cmd()
        .envs(net.env(CONFIG))
        .args(["--transport-cmd", &remote.transport(), "devbox", "--list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "no sessions on devbox\n"
    );
}

/// A client of `name` connected through devbox.lan, with fast redials.
fn redialling(remote: &Remote, net: &Net, name: &str) -> Client {
    let mut env = net.env(CONFIG);
    for (k, v) in [
        ("ACS_BACKOFF_MS", "100"),
        ("ACS_PING_MS", "200"),
        ("ACS_DEAD_MS", "800"),
    ] {
        env.push((k.into(), v.into()));
    }
    let mut c = Client::start_env(
        remote,
        &[name, "r", "--", "/bin/sh", "-c", "echo up; sleep 30"],
        &refs(&env),
    );
    c.wait_for("up", T);
    c
}

#[test]
fn a_redial_resolves_the_alias_again() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let mut c = redialling(&remote, &net, "devbox");
    // The network changes: only the outside address answers now.
    net.set_up(&["devbox.example.com"]);
    remote.cut_link();
    c.wait_for(
        "devbox: now using me@devbox.example.com (was devbox.lan)",
        T,
    );
    remote.wait_connections(2, T);
    // The fake remote is one machine, so the session resumes.
    c.send(b"echo back\r");
    c.wait_for("back", T);
    c.send(&command(b'x'));
    c.wait(T);
}

#[test]
fn a_redial_of_user_at_alias_keeps_the_user() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let mut c = redialling(&remote, &net, "you@devbox");
    net.set_up(&["devbox.example.com"]);
    remote.cut_link();
    c.wait_for(
        "you@devbox: now using you@devbox.example.com (was you@devbox.lan)",
        T,
    );
    remote.wait_connections(2, T);
    c.send(b"echo back\r");
    c.wait_for("back", T);
    // The hints name the host as given, user and all.
    c.send(&command(b'd'));
    c.wait(T);
    c.wait_for("acs you@devbox r", T);
}
