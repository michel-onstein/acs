//! Host aliases (acs-ue2, DESIGN §7.3). A fake `ping` (`ACS_PING`, `Net` in
//! tests/common) answers for the hosts listed in a file, so no ICMP is
//! needed. The fake transport ignores the destination, so `-v` output shows
//! which one was chosen.

mod common;

use common::*;

const CONFIG: &str = "\
aliases:
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

/// The program has started: the session announcement first, then its
/// `up`.
///
/// **Every client in this file runs under `-v`**, which echoes the whole
/// remote prelude into the terminal before the dial is answered — and the
/// word `up` is two letters that any of that kilobyte of shell script may
/// contain. It has happened: a refusal naming the *group* write bit
/// satisfied waits like this one before the connection existed, and
/// `src/ssh.rs` carries a reworded message to this day. `wait_session`
/// puts the echo behind the search cursor, so the `up` found after it is
/// the program's and can be nothing else (acs-ryz).
fn wait_up(c: &mut Client, session: &str) {
    c.wait_session(session);
    c.wait_for("up", T);
}

#[test]
fn an_alias_connects_to_its_first_reachable_host() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan", "devbox.example.com"]);
    net.set_lagging(&["devbox.example.com"]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("devbox"), &refs(&env));
    c.wait_for("devbox: devbox.lan answers ping, using devbox.lan", T);
    wait_up(&mut c, "s");
    // Both were pinged — but devbox.example.com's answer was never waited
    // for, so its line lands whenever its shell runs, here 2 s after
    // devbox.lan has been chosen (acs-uj6).
    net.wait_pinged(&["devbox.example.com", "devbox.lan"]);
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
    wait_up(&mut c, "s");
}

/// Regression (acs-4pv): macOS ping cannot reach an IPv6 host at all — it
/// exits 68 for one — so an IPv6 host is pinged with ping6 rather than
/// counted as unreachable.
#[test]
fn an_ipv6_host_is_pinged_with_ping6() {
    let remote = Remote::installed();
    let net = Net::new(&["fd00:1::20"]);
    let env = net.env("aliases:\n  nas:\n    - host: \"fd00:1::20\"\n");
    let mut c = Client::start_env(&remote, &session("nas"), &refs(&env));
    c.wait_for("nas: fd00:1::20 answers ping, using fd00:1::20", T);
    wait_up(&mut c, "s");
    // Ping, then ping6.
    net.wait_pinged(&["fd00:1::20", "fd00:1::20"]);
}

#[test]
fn an_unchecked_host_is_used_without_a_ping() {
    let remote = Remote::installed();
    let net = Net::new(&[]);
    let env = net.env(CONFIG);
    let mut c = Client::start_env(&remote, &session("lab"), &refs(&env));
    c.wait_for("lab: using lab2 (reachability_check is off, ", T);
    wait_up(&mut c, "s");
    net.wait_pinged(&["lab1"]);
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

/// devbox.lan would answer, but only after 30 s: at the alias's own 5 s
/// deadline acs gives up on it and takes the next host.
///
/// **The next host is not pinged at all**, and that is deliberate
/// (acs-2dc). It used to be, which quietly gave the alias's deadline a
/// second job: covering the fake ping's own fork, exec and shell startup
/// on a machine running the whole suite. Five seconds is a long time for
/// the first job and not always enough for the second — eight runs in
/// sixty-three failed here under load, with the fallback reported down
/// because its shell had not been scheduled yet. `reachability_check:
/// false` takes the deadline out of the fallback's way altogether. The
/// pinged fallback is `falls_back_to_the_next_host_and_its_user`'s to
/// cover, and its deadline is the minute [`Net::env`] gives a
/// configuration that sets none.
///
/// **What is left of the clock is one lower bound**, which load cannot
/// break: starving the machine only makes the give-up later, never
/// earlier. The two things the old `took < 25s` stood for are said by the
/// output instead, and said better:
///
/// - *the alias's 5 s was used, not the file's 20 s* — the line names the
///   deadline it applied, so a global one would read `within 20s` and the
///   wait for this one would fail;
/// - *the 30 s ping was not waited out* — had it been, devbox.lan answers
///   and is chosen, and the line below is never printed at all.
#[test]
fn a_host_slower_than_the_deadline_is_passed_over() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    net.set_slow(&["devbox.lan"]);
    let config = "\
reachability_timeout: 20s
aliases:
  devbox:
    reachability_timeout: 5s
    hosts:
      - host: devbox.lan
      - host: devbox.example.com
        user: me
        reachability_check: false
";
    let env = net.env(config);
    let t0 = std::time::Instant::now();
    let mut c = Client::start_env(&remote, &session("devbox"), &refs(&env));
    c.wait_for("devbox: devbox.lan does not answer ping within 5s", T);
    c.wait_for(
        "devbox: using me@devbox.example.com (reachability_check is off, ",
        T,
    );
    wait_up(&mut c, "s");
    let took = t0.elapsed();
    assert!(took >= std::time::Duration::from_secs(5), "{took:?}");
    // The chosen host was never pinged — which is the line above's own
    // claim, read back from the ping's side. Only the negative half is
    // asserted: the slow host's ping is killed at the deadline, and on a
    // machine this busy the shell may not have reached its own first line
    // by then, so "devbox.lan is in the log" is not a thing this can wait
    // for (acs-2dc). "devbox.example.com is not" only ever fails when
    // something really did ping it.
    assert!(
        !net.pinged().iter().any(|h| h == "devbox.example.com"),
        "{:?}",
        net.pinged()
    );
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
    wait_up(&mut c, "s");
    c.send(&command(b'x'));
    c.wait(T);

    // The fallback host's own `user: me` gives way to the given one.
    net.set_up(&["devbox.example.com"]);
    let mut c = Client::start_env(&remote, &session("you@devbox"), &refs(&env));
    c.wait_for(
        "you@devbox: devbox.example.com answers ping, using you@devbox.example.com",
        T,
    );
    wait_up(&mut c, "s");
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
        wait_up(&mut c, "s");
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
        .args(["list", "--transport-cmd", &remote.transport(), "devbox"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no host for 'devbox'"));

    net.set_up(&["devbox.lan"]);
    let out = acs_cmd()
        .envs(net.env(CONFIG))
        .args(["list", "--transport-cmd", &remote.transport(), "devbox"])
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
    wait_up(&mut c, "r");
    c
}

#[test]
fn a_redial_resolves_the_alias_again() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.lan"]);
    let mut c = redialling(&remote, &net, "devbox");
    // The network changes: only the outside address answers now.
    net.set_up(&["devbox.example.com"]);
    let dialled = remote.connections();
    remote.cut_link();
    c.wait_for(
        "devbox: now using me@devbox.example.com (was devbox.lan)",
        T,
    );
    remote.wait_more_connections(dialled, 1, T);
    // The fake remote is one machine, so the session resumes.
    // The resume's Ctrl-L, echoed by the session's terminal: typing now
    // reaches the program (keys typed during the redial are dropped).
    c.wait_for("^L", T);
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
    let dialled = remote.connections();
    remote.cut_link();
    c.wait_for(
        "you@devbox: now using you@devbox.example.com (was you@devbox.lan)",
        T,
    );
    remote.wait_more_connections(dialled, 1, T);
    // The resume's Ctrl-L, echoed by the session's terminal: typing now
    // reaches the program (keys typed during the redial are dropped).
    c.wait_for("^L", T);
    c.send(b"echo back\r");
    c.wait_for("back", T);
    // The hints name the host as given, user and all.
    c.send(&command(b'd'));
    c.wait(T);
    c.wait_for("acs you@devbox r", T);
}

// ---- identity files (acs-mbd) ----------------------------------------------

/// devbox's first host has its own key; the second takes the alias's.
const KEYED: &str = "\
aliases:
  devbox:
    identity_file: /keys/devbox
    hosts:
      - host: devbox.lan
        identity_file: ~/.ssh/id_lan
      - host: devbox.example.com
";

const HOME: &str = "/home/acs-test";

/// A client of `devbox` on a pty through the recording `ssh`, with `HOME`
/// set and `extra` arguments before the host.
fn keyed_client(ssh: &Ssh, net: &Net, extra: &[&str], env: &[(&str, &str)]) -> Client {
    let mut args = vec![
        "--ssh".to_string(),
        ssh.path().display().to_string(),
        "-v".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(["devbox", "k", "--", "/bin/sh", "-c", "echo up; sleep 30"].map(String::from));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut owned = net.env(KEYED);
    owned.push(("HOME".into(), HOME.into()));
    let mut all = env.to_vec();
    all.extend(refs(&owned));
    Client::spawn(&exe(), &args, &all)
}

fn devbox_ssh(remote: &Remote) -> Ssh {
    Ssh::new(&[("devbox.lan", remote), ("devbox.example.com", remote)])
}

fn key(dest: &str, keys: &[&str]) -> (String, Vec<String>) {
    (dest.into(), keys.iter().map(|k| k.to_string()).collect())
}

#[test]
fn the_command_line_key_beats_the_hosts_which_beats_the_aliass() {
    let remote = Remote::installed();
    let ssh = devbox_ssh(&remote);
    let net = Net::new(&["devbox.lan"]);

    // The entry's own key, with ~ expanded as the shell would.
    let mut c = keyed_client(&ssh, &net, &[], &[]);
    c.wait_for("devbox: identity_file ~/.ssh/id_lan (", T);
    wait_up(&mut c, "k");
    c.send(&command(b'x'));
    c.wait(T);
    assert_eq!(
        ssh.keys().last().unwrap(),
        &key("devbox.lan", &["/home/acs-test/.ssh/id_lan"])
    );

    // The fallback host names none: the alias's.
    net.set_up(&["devbox.example.com"]);
    let mut c = keyed_client(&ssh, &net, &[], &[]);
    c.wait_for("devbox: identity_file /keys/devbox (", T);
    wait_up(&mut c, "k");
    c.send(&command(b'x'));
    c.wait(T);
    assert_eq!(
        ssh.keys().last().unwrap(),
        &key("devbox.example.com", &["/keys/devbox"])
    );

    // -i on the command line, or -o IdentityFile, replaces both.
    net.set_up(&["devbox.lan"]);
    for (opts, want) in [
        (["-i", "/cli/key"], &["/cli/key"][..]),
        (["-o", "IdentityFile=/cli/key"], &[][..]),
    ] {
        let mut c = keyed_client(&ssh, &net, &opts, &[]);
        c.wait_for(
            "devbox: the key given on the command line replaces identity_file ~/.ssh/id_lan (",
            T,
        );
        wait_up(&mut c, "k");
        c.send(&command(b'x'));
        c.wait(T);
        assert_eq!(ssh.keys().last().unwrap(), &key("devbox.lan", want));
    }
}

#[test]
fn a_redial_onto_the_fallback_host_switches_to_its_key() {
    let remote = Remote::installed();
    let ssh = devbox_ssh(&remote);
    let net = Net::new(&["devbox.lan"]);
    let fast = [
        ("ACS_BACKOFF_MS", "100"),
        ("ACS_PING_MS", "200"),
        ("ACS_DEAD_MS", "800"),
    ];
    let mut c = keyed_client(&ssh, &net, &[], &fast);
    wait_up(&mut c, "k");
    net.set_up(&["devbox.example.com"]);
    let dialled = remote.connections();
    remote.cut_link();
    c.wait_for("devbox: now using devbox.example.com (was devbox.lan)", T);
    remote.wait_more_connections(dialled, 1, T);
    // The resume's Ctrl-L, echoed by the session's terminal: typing now
    // reaches the program (keys typed during the redial are dropped).
    c.wait_for("^L", T);
    c.send(b"echo back\r");
    c.wait_for("back", T);
    c.send(&command(b'x'));
    c.wait(T);
    let keys = ssh.keys();
    assert_eq!(keys[0], key("devbox.lan", &["/home/acs-test/.ssh/id_lan"]));
    assert_eq!(
        keys.last().unwrap(),
        &key("devbox.example.com", &["/keys/devbox"])
    );
}

#[test]
fn list_uses_the_chosen_hosts_key() {
    let remote = Remote::installed();
    let ssh = devbox_ssh(&remote);
    let net = Net::new(&["devbox.example.com"]);
    let out = output_of(
        acs_cmd()
            .envs(net.env(KEYED))
            .arg("list")
            .arg("--ssh")
            .arg(ssh.path())
            .arg("devbox"),
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(ssh.keys(), [key("devbox.example.com", &["/keys/devbox"])]);
}

// ---- the host used last on these networks (acs-zpj) -------------------------

/// A state directory of the test's own, and the networks the machine is on
/// (`ACS_NETWATCH_NETS`, which the memory reads as the watcher does).
struct Place {
    dir: acs::testutil::TempDir,
}

impl Place {
    fn new(nets: &str) -> Place {
        let p = Place {
            dir: acs::testutil::TempDir::new(),
        };
        p.move_to(nets);
        p
    }

    fn move_to(&self, nets: &str) {
        std::fs::write(self.dir.path().join("networks"), nets).unwrap();
    }

    fn env(&self, net: &Net) -> Vec<(String, String)> {
        let mut env = net.env(CONFIG);
        let dir = self.dir.path();
        env.push(("XDG_STATE_HOME".into(), dir.display().to_string()));
        let nets = dir.join("networks").display().to_string();
        env.push(("ACS_NETWATCH_NETS".into(), nets));
        env
    }

    fn remembered(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("acs/aliases")).unwrap_or_default()
    }
}

/// Start a `devbox` session, wait for each of `lines`, and end it.
fn connect_through(remote: &Remote, env: &[(String, String)], lines: &[&str]) -> String {
    let mut c = Client::start_env(remote, &session("devbox"), &refs(env));
    for l in lines {
        c.wait_for(l, T);
    }
    wait_up(&mut c, "s");
    c.send(&command(b'x'));
    c.wait(T);
    c.text()
}

#[test]
fn the_host_used_last_on_these_networks_is_taken_without_waiting() {
    let remote = Remote::installed();
    let net = Net::new(&["devbox.example.com"]);
    let place = Place::new("192.168.1.5/24\n");
    let env = place.env(&net);
    // The first time devbox.lan is tried and passed over, and the fallback
    // is remembered for this network.
    connect_through(
        &remote,
        &env,
        &[
            "devbox: devbox.lan does not answer ping",
            "devbox: devbox.example.com answers ping, using me@devbox.example.com",
        ],
    );
    assert_eq!(
        place.remembered(),
        "devbox\tdevbox.example.com\t192.168.1.0/24\n"
    );
    // Now devbox.lan would take 30 s to say it is down, and the deadline
    // is a minute: the remembered host is taken as soon as it answers, and
    // devbox.lan is not waited for.
    net.set_slow(&["devbox.lan"]);
    let text = connect_through(
        &remote,
        &env,
        &[
            "devbox: devbox.example.com was used last on these networks",
            "devbox: devbox.example.com answers ping, using me@devbox.example.com",
            "devbox: devbox.lan not waited for: devbox.example.com was used last on these networks",
        ],
    );
    assert!(!text.contains("devbox.lan does not answer"), "{text}");
    // On another network the memory is not this one's: devbox.lan is
    // tried again, and that network gets a memory of its own.
    net.set_slow(&[]);
    place.move_to("10.1.0.5/16\n");
    let text = connect_through(
        &remote,
        &env,
        &[
            "devbox: devbox.lan does not answer ping",
            "devbox: devbox.example.com answers ping, using me@devbox.example.com",
        ],
    );
    assert!(!text.contains("used last"), "{text}");
    assert_eq!(
        place.remembered(),
        "devbox\tdevbox.example.com\t10.1.0.0/16\n\
         devbox\tdevbox.example.com\t192.168.1.0/24\n"
    );
}

// ---- reachability_tcp: the ssh port instead of a ping (acs-fbes) -----------

/// A fake `ssh` for `ACS_SSH` whose `-G` sends `devbox.lan` to a closed
/// port, `me@devbox.example.com` to `open`, and `jumped` through a
/// ProxyJump. The session itself goes over the fake transport, so this is
/// only ever asked `-G`.
fn port_ssh(dir: &acs::testutil::TempDir, open: u16, closed: u16) -> String {
    let path = dir.path().join("ssh");
    let body = format!(
        "#!/bin/sh\n[ \"$1\" = -G ] || exit 2\nfor d; do :; done\ncase \"$d\" in\n\
         devbox.lan) printf 'hostname 127.0.0.1\\nport {closed}\\n' ;;\n\
         me@devbox.example.com) printf 'hostname 127.0.0.1\\nport {open}\\n' ;;\n\
         jumped) printf 'hostname 10.9.9.9\\nport 22\\nproxyjump bastion\\n' ;;\n\
         *) exit 255 ;;\n\
         esac\n"
    );
    write_executable(&path, body);
    path.display().to_string()
}

/// A listening port and a closed one, on 127.0.0.1.
fn ports() -> (std::net::TcpListener, u16, u16) {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let open_port = open.local_addr().unwrap().port();
    (open, open_port, closed_port)
}

#[test]
fn reachability_tcp_checks_the_ssh_port_instead_of_pinging() {
    let remote = Remote::installed();
    // Both answer a ping: only the ports can tell them apart.
    let net = Net::new(&["devbox.lan", "devbox.example.com"]);
    let dir = acs::testutil::TempDir::new();
    let (_listener, open, closed) = ports();
    let mut env = net.env(&format!("reachability_tcp: true\n{CONFIG}"));
    env.push(("ACS_SSH".into(), port_ssh(&dir, open, closed)));
    let mut c = Client::start_env(&remote, &session("devbox"), &refs(&env));
    c.wait_for(
        &format!("devbox: devbox.lan does not answer on port {closed} within 60s"),
        T,
    );
    c.wait_for(
        &format!("devbox: devbox.example.com answers on port {open}, using me@devbox.example.com"),
        T,
    );
    wait_up(&mut c, "s");
    assert!(net.pinged().is_empty(), "{:?}", net.pinged());
}

/// An entry ssh reaches through a ProxyJump has no port of its own to try,
/// so it is pinged as without the setting.
#[test]
fn reachability_tcp_pings_a_host_behind_a_proxy_jump() {
    let remote = Remote::installed();
    let net = Net::new(&["jumped"]);
    let dir = acs::testutil::TempDir::new();
    let (_listener, open, closed) = ports();
    let mut env = net.env("reachability_tcp: true\naliases:\n  inside: [{host: jumped}]\n");
    env.push(("ACS_SSH".into(), port_ssh(&dir, open, closed)));
    let mut c = Client::start_env(&remote, &session("inside"), &refs(&env));
    c.wait_for(
        "inside: jumped answers ping (ssh reaches it through a proxy), using jumped",
        T,
    );
    wait_up(&mut c, "s");
    net.wait_pinged(&["jumped"]);
}
