//! Host aliases (DESIGN §7.3): `acs [user@]<alias>` connects to the first of
//! the alias's configured hosts that answers a ping, or that is not checked.
//! The hosts are pinged at once, and their rank decides: on a network this
//! machine is on (`prefer_local_network`), then `prefer: true`, then the
//! configured order.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::config::{format_timeout, Alias, Config, HostEntry};
use crate::netmatch::{self, LocalNet, Network};

/// Whether a host answers a ping within the deadline given. Shared with the
/// threads that ping an alias's hosts at once.
pub type Reachable = dyn Fn(&str, Duration) -> bool + Send + Sync;

/// The entry `name` stands for: `Ok(None)` if it is not an alias, the chosen
/// entry otherwise, and an error naming every host tried when none answers.
///
/// `user@<alias>` goes through the alias as that user: the chosen entry's
/// `user` is the given one, whatever the entry says. An entry without an
/// `identity_file` takes the alias's. `reachable` pings one host; `network`
/// gives this machine's networks and a resolver, asked only when the alias
/// has `prefer_local_network`; `log` receives why each entry was taken or
/// skipped.
pub fn resolve(
    name: &str,
    config: &Config,
    reachable: Arc<Reachable>,
    network: &dyn Fn() -> Network,
    log: &mut dyn FnMut(String),
) -> Result<Option<HostEntry>, String> {
    let (user, alias) = split_user(name);
    let Some(alias) = config.alias(alias) else {
        return Ok(None);
    };
    if let Some(u) = user {
        log(format!("{name}: logging in as {u}, from the command line"));
    }
    let timeout = config.reachability_timeout_for(alias).value;
    let network = config
        .prefer_local_network_for(alias)
        .value
        .then(network)
        .filter(|n| !n.local.is_empty());
    pick(name, alias, user, timeout, reachable, network, log).map(Some)
}

/// `user@host` as its login name and host, split at the last `@` as ssh
/// does; a name without one (or with an empty user) has no login name.
pub fn split_user(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        _ => (None, name),
    }
}

/// The entry to use. The entries are ranked ([`rank`]); every checked host
/// that could be chosen is pinged at once, and the k-th ranked host is taken
/// as soon as it has answered and every one ranked before it has not — what
/// trying them one after another would choose, in at most one `timeout`.
/// With `network`, the host names are resolved meanwhile, by the same
/// deadline, to rank the hosts on this machine's networks first; every
/// checked host is pinged then, since the rank is known only once they are.
fn pick(
    name: &str,
    alias: &Alias,
    user: Option<&str>,
    timeout: Duration,
    reachable: Arc<Reachable>,
    network: Option<Network>,
    log: &mut dyn FnMut(String),
) -> Result<HostEntry, String> {
    let entries: Vec<HostEntry> = alias
        .entries
        .iter()
        .map(|e| HostEntry {
            user: user.map(String::from).or_else(|| e.user.clone()),
            identity_file: e
                .identity_file
                .clone()
                .or_else(|| alias.identity_file.clone()),
            ..e.clone()
        })
        .collect();
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    let ping = |i: usize| {
        let (tx, host, reachable) = (tx.clone(), entries[i].host.clone(), Arc::clone(&reachable));
        // Not joined: once a host is chosen nobody waits for the others,
        // whose pings give up at the deadline by themselves.
        std::thread::spawn(move || {
            let _ = tx.send((i, reachable(&host, timeout)));
        });
    };
    // Nothing ranked after an unchecked host can be chosen, so nothing
    // there is pinged.
    let candidates = |order: Vec<usize>| -> Vec<usize> {
        match order.iter().position(|&i| !entries[i].reachability_check) {
            Some(p) => order[..=p].to_vec(),
            None => order,
        }
    };
    let (order, on_net) = match network {
        None => {
            let order = candidates(rank(&entries, &[]));
            for &i in &order {
                if entries[i].reachability_check {
                    ping(i);
                }
            }
            (order, vec![None; entries.len()])
        }
        Some(n) => {
            for (i, e) in entries.iter().enumerate() {
                if e.reachability_check {
                    ping(i);
                }
            }
            let on_net = locate(&entries, &n, deadline);
            for (e, net) in entries.iter().zip(&on_net) {
                if let Some(net) = net {
                    log(format!("{name}: {} is on the local network {net}", e.host));
                }
            }
            (candidates(rank(&entries, &on_net)), on_net)
        }
    };
    drop(tx);
    let mut answered = vec![None; entries.len()];
    let mut tried = Vec::new();
    for &i in &order {
        let e = &entries[i];
        let dest = e.destination();
        let mut why: Vec<String> = Vec::new();
        if let Some(net) = on_net[i] {
            why.push(format!("on {net}"));
        }
        if e.prefer {
            why.push("preferred".into());
        }
        if !e.reachability_check {
            why.insert(0, "reachability_check is off".into());
            why.push(e.origin.to_string());
            log(format!("{name}: using {dest} ({})", why.join(", ")));
            return Ok(e.clone());
        }
        // An answer after the deadline counts as none.
        while answered[i].is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((j, up)) => answered[j] = Some(up),
                Err(_) => break,
            }
        }
        if answered[i] == Some(true) {
            let why = match why.is_empty() {
                true => String::new(),
                false => format!(" ({})", why.join(", ")),
            };
            log(format!(
                "{name}: {} answers ping, using {dest}{why}",
                e.host
            ));
            return Ok(e.clone());
        }
        log(format!(
            "{name}: {} does not answer ping within {}",
            e.host,
            format_timeout(timeout)
        ));
        tried.push(e.host.clone());
    }
    Err(format!(
        "no host for '{name}' is reachable (tried {})",
        tried.join(", ")
    ))
}

/// The local network each entry's host is on, if any: every name resolved
/// at once by `network`'s resolver, those not resolved by `deadline`
/// counting as on none.
fn locate(entries: &[HostEntry], network: &Network, deadline: Instant) -> Vec<Option<LocalNet>> {
    let (tx, rx) = mpsc::channel();
    let left = deadline.saturating_duration_since(Instant::now());
    for (i, e) in entries.iter().enumerate() {
        let (tx, host, resolve) = (tx.clone(), e.host.clone(), Arc::clone(&network.resolve));
        std::thread::spawn(move || {
            let _ = tx.send((i, resolve(&host, left)));
        });
    }
    drop(tx);
    let mut on_net = vec![None; entries.len()];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, addrs)) => on_net[i] = netmatch::matching(&network.local, &addrs),
            Err(_) => break,
        }
    }
    on_net
}

/// The indices of `entries` in the order they are tried: those on a local
/// network (`on_net`, empty for none) first, then those with
/// `prefer: true`, then the rest — each group in configured order.
fn rank(entries: &[HostEntry], on_net: &[Option<LocalNet>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| {
        let local = on_net.get(i).is_some_and(|n| n.is_some());
        (!local, !entries[i].prefer)
    });
    order
}

/// An alias's entries in the order they are tried when no local network is
/// looked at (DESIGN §7.3): those with `prefer: true` first, then the rest,
/// each in configured order.
pub fn ranked(entries: &[HostEntry]) -> Vec<&HostEntry> {
    rank(entries, &[])
        .into_iter()
        .map(|i| &entries[i])
        .collect()
}

/// Ping `host` once, waiting at most `deadline` for the answer (`ACS_PING`
/// names the program).
pub fn ping(host: &str, deadline: Duration) -> bool {
    let prog = std::env::var_os("ACS_PING")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "ping".into());
    ping_with(&prog, host, deadline)
}

fn ping_with(prog: &OsStr, host: &str, deadline: Duration) -> bool {
    let mut c = Command::new(prog);
    // One packet. The deadline is acs's own, to the millisecond: macOS -t
    // and BusyBox -W take whole seconds, and iputils -W fractions only in
    // newer releases. The one given to ping, a second or more past it, is a
    // backstop should acs not get to kill it.
    let flag = if cfg!(target_os = "macos") {
        "-t"
    } else {
        "-W"
    };
    let backstop = (deadline.as_secs() + 2).to_string();
    c.args(["-c", "1", flag, &backstop, "--", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Spawned apart from waited for: several threads ping at once.
    let Ok(mut child) = crate::sys::spawn(&mut c) else {
        return false;
    };
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::sync::Mutex;

    /// No local network and no resolver: what the aliases without
    /// `prefer_local_network` never ask for.
    fn no_network() -> Network {
        Network {
            local: Vec::new(),
            resolve: Arc::new(|_: &str, _: Duration| Vec::new()),
        }
    }

    fn config(yaml: &str) -> Config {
        let dir = TempDir::new();
        let f = dir.path().join("c.yaml");
        std::fs::write(&f, yaml).unwrap();
        Config::load_files(&[f]).unwrap()
    }

    const TWO: &str = "\
hosts:
  devbox:
    - host: devbox.lan
    - host: devbox.example.com
      user: me
  lab:
    - host: lab1
    - host: lab2
      reachability_check: false
    - host: lab3
";

    /// A fake ping: each host answers (`true`) or not after its delay in
    /// milliseconds, and every host asked about is recorded with the
    /// deadline it was given.
    struct Fake {
        hosts: Vec<(String, u64, bool)>,
        asked: Mutex<Vec<(String, Duration)>>,
    }

    impl Fake {
        fn new(hosts: &[(&str, u64, bool)]) -> Arc<Fake> {
            Arc::new(Fake {
                hosts: hosts
                    .iter()
                    .map(|&(h, ms, up)| (h.to_string(), ms, up))
                    .collect(),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn up(up: &[&str]) -> Arc<Fake> {
            Fake::new(&up.iter().map(|&h| (h, 0, true)).collect::<Vec<_>>())
        }

        /// The closure for `resolve`.
        fn reachable(self: &Arc<Fake>) -> Arc<Reachable> {
            let f = Arc::clone(self);
            Arc::new(move |h: &str, deadline| {
                f.asked.lock().unwrap().push((h.to_string(), deadline));
                match f.hosts.iter().find(|(name, ..)| name == h) {
                    Some(&(_, ms, up)) => {
                        std::thread::sleep(Duration::from_millis(ms));
                        up
                    }
                    None => false,
                }
            })
        }

        /// Every host asked about, sorted, once every ping has returned.
        fn pinged(self: &Arc<Fake>) -> Vec<String> {
            // The pinging threads each hold the closure, which holds `self`.
            let end = Instant::now() + Duration::from_secs(10);
            while Arc::strong_count(self) > 1 && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut v: Vec<String> = self
                .asked
                .lock()
                .unwrap()
                .iter()
                .map(|a| a.0.clone())
                .collect();
            v.sort();
            v
        }
    }

    /// Resolve with `up` as the hosts that answer; returns the chosen
    /// destination, the hosts pinged (sorted), and the log.
    fn run(name: &str, up: &[&str]) -> (Result<Option<String>, String>, Vec<String>, Vec<String>) {
        let c = config(TWO);
        let fake = Fake::up(up);
        let mut log = Vec::new();
        let r = resolve(name, &c, fake.reachable(), &no_network, &mut |m| {
            log.push(m)
        });
        (r.map(|e| e.map(|e| e.destination())), fake.pinged(), log)
    }

    /// Resolve `name` in `yaml` against `fake`; returns the chosen
    /// destination, the log, and how long it took.
    fn timed(
        yaml: &str,
        name: &str,
        fake: &Arc<Fake>,
    ) -> (Result<String, String>, Vec<String>, Duration) {
        let c = config(yaml);
        let mut log = Vec::new();
        let start = Instant::now();
        let r = resolve(name, &c, fake.reachable(), &no_network, &mut |m| {
            log.push(m)
        });
        let took = start.elapsed();
        (r.map(|e| e.unwrap().destination()), log, took)
    }

    const ABC: &str = "\
hosts:
  abc:
    - host: a
    - host: b
    - host: c
";

    #[test]
    fn a_name_that_is_not_an_alias_is_left_alone() {
        for name in ["other", "me@other", "@devbox", "devbox@"] {
            let (r, pinged, log) = run(name, &[]);
            assert_eq!(r, Ok(None), "{name}");
            assert!(pinged.is_empty(), "{name}");
            assert!(log.is_empty(), "{name}");
        }
    }

    #[test]
    fn a_login_name_is_split_at_the_last_at() {
        assert_eq!(split_user("devbox"), (None, "devbox"));
        assert_eq!(split_user("me@devbox"), (Some("me"), "devbox"));
        assert_eq!(split_user("me@corp@devbox"), (Some("me@corp"), "devbox"));
        assert_eq!(split_user("@devbox"), (None, "@devbox"));
    }

    #[test]
    fn user_at_alias_logs_in_as_that_user_on_the_first_host() {
        let (r, pinged, log) = run("you@devbox", &["devbox.lan"]);
        assert_eq!(r, Ok(Some("you@devbox.lan".into())));
        assert_eq!(pinged, ["devbox.example.com", "devbox.lan"]);
        assert_eq!(
            log,
            [
                "you@devbox: logging in as you, from the command line",
                "you@devbox: devbox.lan answers ping, using you@devbox.lan",
            ]
        );
    }

    #[test]
    fn user_at_alias_replaces_the_fallback_hosts_own_user() {
        let (r, pinged, _) = run("you@devbox", &["devbox.example.com"]);
        assert_eq!(r, Ok(Some("you@devbox.example.com".into())));
        assert_eq!(pinged, ["devbox.example.com", "devbox.lan"]);
        let (r, _, _) = run("you@lab", &[]);
        assert_eq!(r, Ok(Some("you@lab2".into())));
        let (r, _, _) = run("you@devbox", &[]);
        assert_eq!(
            r,
            Err(
                "no host for 'you@devbox' is reachable (tried devbox.lan, devbox.example.com)"
                    .into()
            )
        );
    }

    #[test]
    fn the_first_host_that_answers_is_used() {
        let (r, pinged, log) = run("devbox", &["devbox.lan", "devbox.example.com"]);
        assert_eq!(r, Ok(Some("devbox.lan".into())));
        // Both were pinged, at once; the configured order decides.
        assert_eq!(pinged, ["devbox.example.com", "devbox.lan"]);
        assert_eq!(log, ["devbox: devbox.lan answers ping, using devbox.lan"]);
    }

    #[test]
    fn falls_back_to_the_next_host_with_its_user() {
        let (r, pinged, log) = run("devbox", &["devbox.example.com"]);
        assert_eq!(r, Ok(Some("me@devbox.example.com".into())));
        assert_eq!(pinged, ["devbox.example.com", "devbox.lan"]);
        assert_eq!(
            log[0],
            "devbox: devbox.lan does not answer ping within 500ms"
        );
        assert!(log[1].ends_with("using me@devbox.example.com"), "{log:?}");
    }

    #[test]
    fn an_unchecked_host_is_taken_without_a_ping() {
        let (r, pinged, log) = run("lab", &[]);
        assert_eq!(r, Ok(Some("lab2".into())));
        assert_eq!(pinged, ["lab1"]);
        assert!(
            log[1].starts_with("lab: using lab2 (reachability_check is off, "),
            "{log:?}"
        );
    }

    #[test]
    fn an_entry_without_a_key_takes_the_aliass() {
        let c = config(
            "\
hosts:
  devbox:
    identity_file: ~/.ssh/id_alias
    hosts:
      - host: devbox.lan
        identity_file: ~/.ssh/id_lan
      - host: devbox.example.com
  plain:
    - host: plain.lan
",
        );
        let key = |name: &str, up: &[&str]| {
            resolve(name, &c, Fake::up(up).reachable(), &no_network, &mut |_| {})
                .unwrap()
                .unwrap()
                .identity_file
                .map(|s| format!("{}:{}", s.value, s.origin.unwrap().line))
        };
        // The entry's own key beats the alias's; the fallback has none and
        // takes the alias's, whoever logs in.
        assert_eq!(
            key("devbox", &["devbox.lan"]).as_deref(),
            Some("~/.ssh/id_lan:6")
        );
        assert_eq!(
            key("devbox", &["devbox.example.com"]).as_deref(),
            Some("~/.ssh/id_alias:3")
        );
        assert_eq!(
            key("you@devbox", &["devbox.example.com"]).as_deref(),
            Some("~/.ssh/id_alias:3")
        );
        assert_eq!(key("plain", &["plain.lan"]), None);
    }

    #[test]
    fn none_reachable_names_every_host_tried() {
        let (r, pinged, _) = run("devbox", &[]);
        assert_eq!(
            r,
            Err("no host for 'devbox' is reachable (tried devbox.lan, devbox.example.com)".into())
        );
        assert_eq!(pinged.len(), 2);
    }

    #[test]
    fn a_later_host_answering_first_does_not_beat_an_earlier_one_in_time() {
        // b answers at once, a after 150ms: a is first in the list and
        // answers within the deadline, so a it is.
        let fake = Fake::new(&[("a", 150, true), ("b", 0, true), ("c", 0, true)]);
        let (r, log, took) = timed(ABC, "abc", &fake);
        assert_eq!(r, Ok("a".into()));
        assert_eq!(log, ["abc: a answers ping, using a"]);
        assert!(took >= Duration::from_millis(150), "{took:?}");
        // And b, answering, is taken over a c that answered first.
        let fake = Fake::new(&[("a", 0, false), ("b", 150, true), ("c", 0, true)]);
        let (r, log, _) = timed(ABC, "abc", &fake);
        assert_eq!(r, Ok("b".into()));
        assert_eq!(
            log,
            [
                "abc: a does not answer ping within 500ms",
                "abc: b answers ping, using b",
            ]
        );
    }

    #[test]
    fn the_hosts_are_pinged_at_once() {
        // One after another, three 300ms pings take 900ms.
        let fake = Fake::new(&[("a", 300, false), ("b", 300, false), ("c", 300, true)]);
        let (r, _, took) = timed(&format!("reachability_timeout: 2s\n{ABC}"), "abc", &fake);
        assert_eq!(r, Ok("c".into()));
        assert!(took < Duration::from_millis(700), "{took:?}");
        assert_eq!(fake.pinged(), ["a", "b", "c"]);
    }

    #[test]
    fn an_answer_after_the_deadline_counts_as_none() {
        let yaml = format!("reachability_timeout: 100ms\n{ABC}");
        let fake = Fake::new(&[("a", 2000, true), ("b", 0, true)]);
        let (r, log, took) = timed(&yaml, "abc", &fake);
        assert_eq!(r, Ok("b".into()));
        assert_eq!(log[0], "abc: a does not answer ping within 100ms");
        assert!(took < Duration::from_millis(1000), "{took:?}");
        // Nothing answering in time takes one deadline, not one per host.
        let fake = Fake::new(&[("a", 2000, true), ("b", 2000, true), ("c", 2000, true)]);
        let (r, _, took) = timed(&yaml, "abc", &fake);
        assert_eq!(
            r,
            Err("no host for 'abc' is reachable (tried a, b, c)".into())
        );
        assert!(took < Duration::from_millis(1000), "{took:?}");
    }

    /// acs-o96: hosts `a`, `b`, `c` with `prefer: true` on the ones named.
    fn preferring(preferred: &[&str], unchecked: &[&str]) -> String {
        let mut yaml = String::from("hosts:\n  abc:\n");
        for h in ["a", "b", "c"] {
            yaml.push_str(&format!("    - host: {h}\n"));
            if preferred.contains(&h) {
                yaml.push_str("      prefer: true\n");
            }
            if unchecked.contains(&h) {
                yaml.push_str("      reachability_check: false\n");
            }
        }
        yaml
    }

    #[test]
    fn a_preferred_host_beats_an_earlier_one_that_answered_first() {
        // a answers at once, c (preferred) after 150ms: c it is.
        let fake = Fake::new(&[("a", 0, true), ("b", 0, true), ("c", 150, true)]);
        let (r, log, _) = timed(&preferring(&["c"], &[]), "abc", &fake);
        assert_eq!(r, Ok("c".into()));
        assert_eq!(log, ["abc: c answers ping, using c (preferred)"]);
        // A preferred host that does not answer: the configured order.
        let fake = Fake::new(&[("a", 0, false), ("b", 0, true), ("c", 0, false)]);
        let (r, log, _) = timed(&preferring(&["c"], &[]), "abc", &fake);
        assert_eq!(r, Ok("b".into()));
        assert_eq!(
            log,
            [
                "abc: c does not answer ping within 500ms",
                "abc: a does not answer ping within 500ms",
                "abc: b answers ping, using b",
            ]
        );
        // None answering: every host named, the preferred first.
        let fake = Fake::new(&[]);
        let (r, _, _) = timed(&preferring(&["b"], &[]), "abc", &fake);
        assert_eq!(
            r,
            Err("no host for 'abc' is reachable (tried b, a, c)".into())
        );
    }

    #[test]
    fn several_preferred_hosts_go_by_order_and_an_unchecked_one_wins_outright() {
        // b and c preferred and both answering: b, the first of them.
        let fake = Fake::new(&[("a", 0, true), ("b", 100, true), ("c", 0, true)]);
        let (r, _, _) = timed(&preferring(&["b", "c"], &[]), "abc", &fake);
        assert_eq!(r, Ok("b".into()));
        // b down: c, still ahead of a.
        let fake = Fake::new(&[("a", 0, true), ("b", 0, false), ("c", 0, true)]);
        let (r, _, _) = timed(&preferring(&["b", "c"], &[]), "abc", &fake);
        assert_eq!(r, Ok("c".into()));
        // A preferred host that is never pinged is used at once: nothing is
        // pinged at all.
        let fake = Fake::new(&[("a", 0, true)]);
        let (r, log, _) = timed(&preferring(&["c"], &["c"]), "abc", &fake);
        assert_eq!(r, Ok("c".into()));
        assert!(
            log[0].starts_with("abc: using c (reachability_check is off, preferred, "),
            "{log:?}"
        );
        assert!(fake.pinged().is_empty(), "{:?}", fake.pinged());
        // Ranked: preferred first, each group in configured order.
        let c = config(&preferring(&["b", "c"], &[]));
        let ranked: Vec<&str> = ranked(&c.alias("abc").unwrap().entries)
            .iter()
            .map(|e| e.host.as_str())
            .collect();
        assert_eq!(ranked, ["b", "c", "a"]);
    }

    // ---- prefer_local_network (acs-sia) ------------------------------------

    /// devbox.example.com first, devbox.lan second, as a home alias lists
    /// them; `extra` goes into the alias's settings.
    fn home(extra: &str) -> String {
        format!(
            "hosts:\n  devbox:\n{extra}    hosts:\n      - host: devbox.example.com\n      - host: devbox.lan\n"
        )
    }

    /// This machine on `local` (`addr/prefix`), and a resolver answering
    /// `names` after `delay_ms`.
    fn on(local: &[&str], names: &[(&str, &str)], delay_ms: u64) -> Network {
        let local = local
            .iter()
            .map(|s| {
                let (a, p) = s.split_once('/').unwrap();
                LocalNet::new(a.parse().unwrap(), p.parse().unwrap())
            })
            .collect();
        let names: Vec<(String, std::net::IpAddr)> = names
            .iter()
            .map(|(n, a)| (n.to_string(), a.parse().unwrap()))
            .collect();
        Network {
            local,
            resolve: Arc::new(move |host: &str, _| {
                std::thread::sleep(Duration::from_millis(delay_ms));
                names
                    .iter()
                    .filter(|(n, _)| n == host)
                    .map(|(_, a)| *a)
                    .collect()
            }),
        }
    }

    /// Resolve `devbox` in `yaml` with every host answering as `fake` says
    /// and `network` as this machine's; the destination and the log.
    fn located(
        yaml: &str,
        fake: &Arc<Fake>,
        network: Network,
    ) -> (Result<String, String>, Vec<String>) {
        let c = config(yaml);
        let mut log = Vec::new();
        let network = std::sync::Mutex::new(Some(network));
        let r = resolve(
            "devbox",
            &c,
            fake.reachable(),
            &|| network.lock().unwrap().take().expect("asked twice"),
            &mut |m| log.push(m),
        );
        (r.map(|e| e.unwrap().destination()), log)
    }

    const HOME_NAMES: &[(&str, &str)] = &[
        ("devbox.lan", "192.168.1.20"),
        ("devbox.lan", "fd00:1::20"),
        ("devbox.example.com", "203.0.113.9"),
    ];

    #[test]
    fn a_host_on_a_local_network_goes_first_in_either_family() {
        let yaml = home("    prefer_local_network: true\n");
        let both = || Fake::up(&["devbox.example.com", "devbox.lan"]);
        // IPv4: at home on 192.168.1.0/24, devbox.lan (listed second) it is.
        let (r, log) = located(&yaml, &both(), on(&["192.168.1.5/24"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.lan".into()));
        assert_eq!(
            log,
            [
                "devbox: devbox.lan is on the local network 192.168.1.0/24",
                "devbox: devbox.lan answers ping, using devbox.lan (on 192.168.1.0/24)",
            ]
        );
        // IPv6 alone matches too.
        let (r, log) = located(&yaml, &both(), on(&["fd00:1::5/64"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.lan".into()));
        assert!(log[1].ends_with("(on fd00:1::/64)"), "{log:?}");
        // Elsewhere, on no network of theirs: the configured order.
        let (r, _) = located(&yaml, &both(), on(&["10.0.0.5/24"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.example.com".into()));
    }

    #[test]
    fn a_local_host_must_still_answer_and_beats_a_preferred_one() {
        let yaml = home("    prefer_local_network: true\n");
        // On the network but not answering: the next in rank.
        let fake = Fake::up(&["devbox.example.com"]);
        let (r, log) = located(&yaml, &fake, on(&["192.168.1.5/24"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert_eq!(
            log[1],
            "devbox: devbox.lan does not answer ping within 500ms"
        );
        // prefer: true on the other host: the local network still wins.
        let yaml = "hosts:\n  devbox:\n    prefer_local_network: true\n    hosts:\n      - host: devbox.example.com\n        prefer: true\n      - host: devbox.lan\n";
        let fake = Fake::up(&["devbox.example.com", "devbox.lan"]);
        let (r, _) = located(yaml, &fake, on(&["192.168.1.5/24"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.lan".into()));
    }

    #[test]
    fn without_the_setting_or_with_a_slow_resolver_the_order_decides() {
        let fake = Fake::up(&["devbox.example.com", "devbox.lan"]);
        // Off: the network is never asked for (it would panic).
        let c = config(&home(""));
        let r = resolve(
            "devbox",
            &c,
            fake.reachable(),
            &|| panic!("the network was asked for"),
            &mut |_| {},
        );
        assert_eq!(r.unwrap().unwrap().destination(), "devbox.example.com");
        // On, but the names resolve only after the 100ms deadline: no match,
        // and the choice still takes one deadline, not more.
        let yaml = home("    prefer_local_network: true\n    reachability_timeout: 100ms\n");
        let start = Instant::now();
        let (r, _) = located(&yaml, &fake, on(&["192.168.1.5/24"], HOME_NAMES, 1000));
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn the_rank_is_local_network_then_prefer_then_order() {
        let c = config(
            "hosts:\n  x:\n    - host: a\n    - host: b\n      prefer: true\n    - host: c\n    - host: d\n",
        );
        let entries = &c.alias("x").unwrap().entries;
        let net = Some(LocalNet::new("192.168.1.5".parse().unwrap(), 24));
        assert_eq!(rank(entries, &[]), [1, 0, 2, 3]);
        assert_eq!(rank(entries, &[None, None, None, net]), [3, 1, 0, 2]);
        assert_eq!(rank(entries, &[None, net, net, None]), [1, 2, 0, 3]);
    }

    #[test]
    fn the_aliass_own_deadline_is_the_one_used() {
        let yaml = "\
reachability_timeout: 2s
hosts:
  quick:
    reachability_timeout: 250ms
    hosts: [{host: a}]
  plain: [{host: b}]
";
        let fake = Fake::up(&["a", "b"]);
        assert_eq!(timed(yaml, "quick", &fake).0, Ok("a".into()));
        assert_eq!(timed(yaml, "plain", &fake).0, Ok("b".into()));
        fake.pinged();
        let asked = fake.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            [
                ("a".to_string(), Duration::from_millis(250)),
                ("b".to_string(), Duration::from_secs(2))
            ]
        );
    }

    /// An executable `sh` script in `dir`.
    fn script(dir: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn ping_is_one_packet_with_a_backstop_past_the_deadline() {
        let dir = TempDir::new();
        let args = dir.path().join("args");
        let p = script(&dir, "ping", &format!("echo \"$@\" > '{}'", args.display()));
        let flag = if cfg!(target_os = "macos") {
            "-t"
        } else {
            "-W"
        };
        for (ms, secs) in [(500, 2), (1000, 3), (2500, 4)] {
            assert!(ping_with(p.as_os_str(), "h.lan", Duration::from_millis(ms)));
            assert_eq!(
                std::fs::read_to_string(&args).unwrap(),
                format!("-c 1 {flag} {secs} -- h.lan\n")
            );
        }
        let down = script(&dir, "down", "exit 1");
        assert!(!ping_with(down.as_os_str(), "h", Duration::from_secs(1)));
        let none = dir.path().join("no-such-ping");
        assert!(!ping_with(none.as_os_str(), "h", Duration::from_secs(1)));
    }

    #[test]
    fn ping_is_killed_at_the_deadline() {
        let dir = TempDir::new();
        let late = dir.path().join("late");
        let p = script(
            &dir,
            "ping",
            &format!("sleep 1; touch '{}'", late.display()),
        );
        let start = Instant::now();
        assert!(!ping_with(p.as_os_str(), "h", Duration::from_millis(100)));
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "{:?}",
            start.elapsed()
        );
        // Killed, not left to answer late.
        std::thread::sleep(Duration::from_millis(1300));
        assert!(!late.exists());
    }
}
