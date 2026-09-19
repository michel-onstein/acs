//! Ctrl-L after reconnecting (acs-ome, DESIGN §5.2): sent to a session that
//! was already there — on a re-attach and on a resume — once, after the
//! input the drop left unacknowledged, never to a new session or into an
//! open paste, and only as the configuration and environment allow.

mod common;

use std::time::Duration;

use common::*;

/// A program that reports every byte it receives as `in:xx`, as it comes.
const REPORTER: &str = "stty raw -echo; echo ready; \
    while :; do b=$(dd bs=1 count=1 2>/dev/null | od -An -tx1); printf 'in:%s\\n' $b; done";

/// Fast timers so a resume takes a fraction of a second.
const FAST: &[(&str, &str)] = &[
    ("ACS_BACKOFF_MS", "100"),
    ("ACS_PING_MS", "200"),
    ("ACS_DEAD_MS", "800"),
];

fn start(remote: &Remote, host: &str, session: &str, env: &[(&str, &str)]) -> Client {
    let mut c = Client::start_env(
        remote,
        &[host, session, "--", "/bin/sh", "-c", REPORTER],
        env,
    );
    c.wait_for("ready", T);
    c
}

/// The bytes the program reported, in order.
fn received(c: &Client) -> Vec<String> {
    c.text()
        .split("in:")
        .skip(1)
        .filter_map(|s| s.get(..2).map(String::from))
        .collect()
}

/// Type `z` and wait for the program to report it, so everything the client
/// sent before it has arrived.
fn sync(c: &mut Client) {
    c.send(b"z");
    c.wait_for("in:7a", T);
}

/// Start `session`, detach, and attach again from a new client; returns
/// what the program received on the re-attach.
fn reattach(remote: &Remote, host: &str, session: &str, env: &[(&str, &str)]) -> Vec<String> {
    let mut c = start(remote, host, session, env);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0, "{}", c.text());
    let mut c = Client::start_env(remote, &[host, session], env);
    sync(&mut c);
    received(&c)
}

#[test]
fn not_sent_to_a_new_session() {
    let remote = Remote::installed();
    let mut c = start(&remote, "devbox", "new", &[]);
    sync(&mut c);
    assert_eq!(received(&c), ["7a"], "{}", c.text());
}

#[test]
fn sent_once_on_a_reattach_after_the_clear() {
    let remote = Remote::installed();
    let mut c = start(&remote, "devbox", "ra", &[]);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let mut c = Client::start(&remote, &["devbox", "ra"]);
    c.wait_for("in:0c", T);
    sync(&mut c);
    assert_eq!(received(&c), ["0c", "7a"], "{}", c.text());
    // The existing redraw stays: the screen is cleared first.
    let text = c.text();
    let clear = text.find("\x1b[H\x1b[J").expect("the screen is cleared");
    assert!(clear < text.find("in:0c").unwrap(), "{text:?}");
}

#[test]
fn sent_once_on_a_lossless_resume_after_the_resent_input() {
    let remote = Remote::installed();
    let mut c = start(&remote, "devbox", "rs", FAST);
    // Freeze the connection, type into it, then cut it: the keys never
    // reached the master and are resent on the resume.
    let pid = *remote.transport_pids().last().unwrap();
    acs::sys::kill(pid, libc::SIGSTOP).unwrap();
    c.send(b"abc");
    std::thread::sleep(Duration::from_millis(200));
    let _ = acs::sys::kill(pid, libc::SIGKILL);
    remote.wait_connections(2, T);
    c.wait_for("in:0c", T);
    sync(&mut c);
    assert_eq!(received(&c), ["61", "62", "63", "0c", "7a"], "{}", c.text());
}

#[test]
fn not_sent_into_a_paste_the_drop_left_open() {
    let remote = Remote::installed();
    let mut c = start(&remote, "devbox", "rp", FAST);
    // A paste whose end marker never made it before the link was cut.
    c.send(b"\x1b[200~ab");
    c.wait_for("in:62", T);
    remote.cut_link();
    remote.wait_connections(2, T);
    // Resumed: the status line's title is popped.
    c.wait_for("\x1b[23;0t", T);
    sync(&mut c);
    let got = received(&c);
    assert!(!got.contains(&"0c".to_string()), "{got:?}");
    assert_eq!(got.last().map(String::as_str), Some("7a"));
}

#[test]
fn the_setting_the_alias_and_the_environment() {
    let yaml = |global: &str, alias: &str| {
        format!("{global}hosts:\n  devbox:\n{alias}    hosts:\n      - host: devbox.lan\n")
    };
    let off = "redraw_on_reconnect: false\n";
    let alias_on = "    redraw_on_reconnect: true\n";
    let alias_off = "    redraw_on_reconnect: false\n";
    // (configuration, host, environment, Ctrl-L sent)
    type Case<'a> = (String, &'a str, &'a [(&'a str, &'a str)], bool);
    let cases: &[Case] = &[
        (yaml("", ""), "devbox", &[], true),
        (yaml(off, ""), "devbox", &[], false),
        // A host that is not an alias takes the global setting.
        (yaml("", alias_off), "other", &[], true),
        (yaml(off, alias_on), "other", &[], false),
        // The alias's own setting wins over the global one, both ways.
        (yaml(off, alias_on), "devbox", &[], true),
        (yaml("", alias_off), "devbox", &[], false),
        (yaml("", alias_off), "me@devbox", &[], false),
        // The environment wins over both.
        (
            yaml("", alias_off),
            "devbox",
            &[("ACS_REDRAW_ON_RECONNECT", "1")],
            true,
        ),
        (
            yaml("", alias_on),
            "devbox",
            &[("ACS_REDRAW_ON_RECONNECT", "0")],
            false,
        ),
    ];
    for (i, (config, host, extra, sent)) in cases.iter().enumerate() {
        let remote = Remote::installed();
        let net = Net::new(&["devbox.lan"]);
        let mut env = net.env(config);
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let got = reattach(&remote, host, &format!("c{i}"), &refs(&env));
        let want: &[&str] = if *sent { &["0c", "7a"] } else { &["7a"] };
        assert_eq!(got, want, "{host} {extra:?}\n{config}");
    }
}

#[test]
fn one_per_reconnect_a_reattach_then_a_resume() {
    let remote = Remote::installed();
    let mut c = start(&remote, "devbox", "rr", FAST);
    c.send(&command(b'd'));
    assert_eq!(c.wait(T), 0);
    let mut c = Client::start_env(&remote, &["devbox", "rr"], FAST);
    c.wait_for("in:0c", T);
    remote.cut_link();
    remote.wait_connections(3, T);
    c.wait_for("in:0c", T);
    sync(&mut c);
    assert_eq!(received(&c), ["0c", "0c", "7a"], "{}", c.text());
}
