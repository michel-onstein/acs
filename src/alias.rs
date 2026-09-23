//! Host aliases (DESIGN §7.3): `acs [user@]<alias>` connects to the first of
//! the alias's configured hosts that answers a ping, or that is not checked.
//! The hosts are pinged at once, and their rank decides: local first, then
//! `prefer: true`, then the configured order. An entry is local either
//! because the host resolves onto a network this machine's interfaces are
//! on (`prefer_local_network`) or because this machine is on one of the
//! entry's own `local_networks` (acs-9yv). Under `-v` every entry of the
//! alias accounts for itself, the ones the choice never reached included
//! (acs-qis).

use std::ffi::OsStr;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::config::{format_timeout, Alias, Config, HostEntry};
use crate::netmatch::{self, LocalNet, Network};

/// Whether a host answers a ping within the deadline given. Shared with the
/// threads that ping an alias's hosts at once.
pub type Reachable = dyn Fn(&str, Duration) -> bool + Send + Sync;

/// Why an entry is ranked as local — the two directions the match can run
/// (DESIGN §7.3). Both name the network to report; neither exempts the
/// host from its ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    /// `prefer_local_network`: the host resolves onto a network this
    /// machine's interfaces are on.
    Host(LocalNet),
    /// `local_networks` on the entry: one of this machine's own addresses
    /// is on one of them (acs-9yv).
    Caller(LocalNet),
}

/// What the alias is ranked by when locality is looked at at all: this
/// machine's networks (and, for `prefer_local_network`, a resolver).
struct Locality {
    network: Network,
    /// `prefer_local_network`: resolve each host too, and match its
    /// addresses against this machine's networks. Off, nothing is resolved
    /// and the rank is known before any ping.
    by_host: bool,
}

/// The entry `name` stands for: `Ok(None)` if it is not an alias, the chosen
/// entry otherwise, and an error naming every host tried when none answers.
///
/// `user@<alias>` goes through the alias as that user: the chosen entry's
/// `user` is the given one, whatever the entry says. An entry without an
/// `identity_file` takes the alias's. `reachable` pings one host; `network`
/// gives this machine's networks and a resolver, asked only when the alias
/// has `prefer_local_network` or one of its entries has `local_networks`;
/// `log` receives why each entry was taken or skipped.
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
    // This machine's own addresses are needed either way; only
    // `prefer_local_network` also resolves the hosts' names.
    let by_host = config.prefer_local_network_for(alias).value;
    let by_caller = alias.entries.iter().any(|e| !e.local_networks.is_empty());
    let locality = (by_host || by_caller)
        .then(network)
        .filter(|n| !n.local.is_empty())
        .map(|network| Locality { network, by_host });
    pick(name, alias, user, timeout, reachable, locality, log).map(Some)
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
/// With `locality.by_host`, the host names are resolved meanwhile, by the
/// same deadline, to rank the hosts on this machine's networks first; every
/// checked host is pinged then, since the rank is known only once they are.
/// A caller-side `local_networks` needs no lookup, so that rank is known
/// before anything is pinged.
fn pick(
    name: &str,
    alias: &Alias,
    user: Option<&str>,
    timeout: Duration,
    reachable: Arc<Reachable>,
    locality: Option<Locality>,
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
    // The first unchecked entry in the rank: it is taken as soon as it is
    // reached, so nothing ranked after it can ever be chosen.
    let unchecked = |order: &[usize]| order.iter().position(|&i| !entries[i].reachability_check);
    // Nothing ranked after an unchecked host can be chosen, so nothing
    // there is pinged.
    let candidates = |order: &[usize]| -> Vec<usize> {
        match unchecked(order) {
            Some(p) => order[..=p].to_vec(),
            None => order.to_vec(),
        }
    };
    // Caller-side first: it is this machine's own addresses against each
    // entry's networks, so it costs no lookup and no wait.
    let mut on_net: Vec<Option<Local>> = match &locality {
        Some(l) => caller_side(&entries, &l.network),
        None => vec![None; entries.len()],
    };
    // The whole rank, pruned or not: every entry accounts for itself under
    // `-v`, including the ones never tried (acs-qis).
    let ranked = match locality.as_ref().filter(|l| l.by_host) {
        // The rank is already known: only the hosts that could be chosen
        // are pinged.
        None => {
            let ranked = rank(&entries, &on_net);
            for &i in &candidates(&ranked) {
                if entries[i].reachability_check {
                    ping(i);
                }
            }
            ranked
        }
        Some(l) => {
            for (i, e) in entries.iter().enumerate() {
                if e.reachability_check {
                    ping(i);
                }
            }
            locate(&mut on_net, &entries, &l.network, deadline);
            rank(&entries, &on_net)
        }
    };
    let order = candidates(&ranked);
    for (e, local) in entries.iter().zip(&on_net) {
        match local {
            Some(Local::Host(net)) => {
                log(format!("{name}: {} is on the local network {net}", e.host))
            }
            Some(Local::Caller(net)) => log(format!(
                "{name}: this machine is on {net}, so {} is local",
                e.host
            )),
            None => {}
        }
    }
    drop(tx);
    let mut answered = vec![None; entries.len()];
    let mut tried = Vec::new();
    let mut chosen = None;
    for (k, &i) in order.iter().enumerate() {
        let e = &entries[i];
        let dest = e.destination();
        let mut why: Vec<String> = Vec::new();
        match on_net[i] {
            Some(Local::Host(net)) => why.push(format!("on {net}")),
            Some(Local::Caller(net)) => why.push(format!("this machine is on {net}")),
            None => {}
        }
        if e.prefer {
            why.push("preferred".into());
        }
        if !e.reachability_check {
            why.insert(0, "reachability_check is off".into());
            why.push(e.origin.to_string());
            log(format!("{name}: using {dest} ({})", why.join(", ")));
            chosen = Some(k);
            break;
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
            chosen = Some(k);
            break;
        }
        log(format!(
            "{name}: {} does not answer ping within {}",
            e.host,
            format_timeout(timeout)
        ));
        tried.push(e.host.clone());
    }
    // Nothing answered: every entry was tried and has its line already, and
    // the error names them all.
    let Some(k) = chosen else {
        return Err(format!(
            "no host for '{name}' is reachable (tried {})",
            tried.join(", ")
        ));
    };
    untried(name, &entries, &ranked, k, unchecked(&ranked), log);
    Ok(entries[ranked[k]].clone())
}

/// The entries the choice never reached, each with why — the rest of the
/// rank once `ranked[k]` was taken (acs-qis). They come after the "using"
/// line, in rank order, so the whole account reads as the walk that made
/// it. An entry ranked behind an unchecked one that did not itself win was
/// out of the running before any ping, which is the reason worth giving;
/// the others were simply beaten to it.
fn untried(
    name: &str,
    entries: &[HostEntry],
    ranked: &[usize],
    k: usize,
    unchecked: Option<usize>,
    log: &mut dyn FnMut(String),
) {
    for (j, &i) in ranked.iter().enumerate().skip(k + 1) {
        let why = match unchecked {
            Some(p) if j > p && p != k => format!(
                "it is listed after {}, whose reachability_check is off",
                entries[ranked[p]].host
            ),
            _ => format!("{} was chosen first", entries[ranked[k]].host),
        };
        log(format!("{name}: {} not tried: {why}", entries[i].host));
    }
}

/// Each entry whose `local_networks` one of this machine's own addresses
/// falls in (acs-9yv) — the caller-side direction, which needs no name
/// resolution at all. The network reported is the first of the entry's own
/// that matches, so several are taken in configured order.
fn caller_side(entries: &[HostEntry], network: &Network) -> Vec<Option<Local>> {
    let mine: Vec<IpAddr> = network.local.iter().map(|n| n.addr).collect();
    entries
        .iter()
        .map(|e| netmatch::matching(&e.local_networks, &mine).map(Local::Caller))
        .collect()
}

/// The local network each entry's host is on, where it is on one
/// (`prefer_local_network`): every name resolved at once by `network`'s
/// resolver, those not resolved by `deadline` counting as on none. A host
/// match overrides a caller-side one already in `on_net`, being the
/// statement about the host itself.
fn locate(
    on_net: &mut [Option<Local>],
    entries: &[HostEntry],
    network: &Network,
    deadline: Instant,
) {
    let (tx, rx) = mpsc::channel();
    let left = deadline.saturating_duration_since(Instant::now());
    for (i, e) in entries.iter().enumerate() {
        let (tx, host, resolve) = (tx.clone(), e.host.clone(), Arc::clone(&network.resolve));
        std::thread::spawn(move || {
            let _ = tx.send((i, resolve(&host, left)));
        });
    }
    drop(tx);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, addrs)) => {
                if let Some(net) = netmatch::matching(&network.local, &addrs) {
                    on_net[i] = Some(Local::Host(net));
                }
            }
            Err(_) => break,
        }
    }
}

/// The indices of `entries` in the order they are tried: the local ones
/// (`on_net`, empty for none — whichever direction made them local) first,
/// then those with `prefer: true`, then the rest — each group in configured
/// order.
fn rank(entries: &[HostEntry], on_net: &[Option<Local>]) -> Vec<usize> {
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

/// What macOS `ping` exits with when it cannot resolve the host — which it
/// says for every IPv6 address, being IPv4-only (`EX_NOHOST`).
const NO_HOST: i32 = 68;

/// Ping `host` once, waiting at most `deadline` for the answer (`ACS_PING`
/// names the program, `ACS_PING6` the IPv6 one).
pub fn ping(host: &str, deadline: Duration) -> bool {
    let prog = std::env::var_os("ACS_PING")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "ping".into());
    match ping_with(&prog, host, deadline) {
        Some(0) => true,
        // macOS ping is IPv4-only: an IPv6 literal, or a name with only
        // AAAA records, is "cannot resolve" to it. ping6 does those, and
        // takes no timeout flag — our own deadline kills it (acs-4pv).
        // Linux ping is dual-stack and never exits this way.
        Some(NO_HOST) => {
            let prog6 = std::env::var_os("ACS_PING6")
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "ping6".into());
            run_ping(&prog6, &["-c", "1", "--", host], deadline) == Some(0)
        }
        _ => false,
    }
}

/// One ping with the usual arguments; the exit code, or `None` when it had
/// to be killed at the deadline or could not be run.
fn ping_with(prog: &OsStr, host: &str, deadline: Duration) -> Option<i32> {
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
    run_ping(prog, &["-c", "1", flag, &backstop, "--", host], deadline)
}

fn run_ping(prog: &OsStr, args: &[&str], deadline: Duration) -> Option<i32> {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Spawned apart from waited for: several threads ping at once.
    let Ok(mut child) = crate::sys::spawn(&mut c) else {
        return None;
    };
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
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
aliases:
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
aliases:
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
                "you@devbox: devbox.example.com not tried: devbox.lan was chosen first",
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
        assert_eq!(
            log,
            [
                "devbox: devbox.lan answers ping, using devbox.lan",
                "devbox: devbox.example.com not tried: devbox.lan was chosen first",
            ]
        );
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
aliases:
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
        assert_eq!(
            log,
            [
                "abc: a answers ping, using a",
                "abc: b not tried: a was chosen first",
                "abc: c not tried: a was chosen first",
            ]
        );
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
                "abc: c not tried: b was chosen first",
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
        let mut yaml = String::from("aliases:\n  abc:\n");
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
        assert_eq!(
            log,
            [
                "abc: c answers ping, using c (preferred)",
                "abc: a not tried: c was chosen first",
                "abc: b not tried: c was chosen first",
            ]
        );
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
        // The rank is walked to its end: nothing is left after b.
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

    // ---- every entry accounts for itself (acs-qis) -------------------------

    #[test]
    fn the_entries_ranked_after_the_chosen_one_say_who_beat_them() {
        let yaml = "\
aliases:
  devbox:
    - host: devbox.lan
    - host: devbox.vpn
    - host: devbox.example.com
";
        let fake = Fake::up(&["devbox.vpn", "devbox.example.com"]);
        let (r, log, _) = timed(yaml, "devbox", &fake);
        assert_eq!(r, Ok("devbox.vpn".into()));
        assert_eq!(
            log,
            [
                "devbox: devbox.lan does not answer ping within 500ms",
                "devbox: devbox.vpn answers ping, using devbox.vpn",
                "devbox: devbox.example.com not tried: devbox.vpn was chosen first",
            ]
        );
    }

    #[test]
    fn an_entry_behind_an_unchecked_one_says_it_was_never_in_the_running() {
        // a answers, so b (unchecked) was merely beaten to it -- but c
        // could not have been chosen however the pings had gone.
        let yaml = preferring(&[], &["b"]);
        let (r, log, _) = timed(&yaml, "abc", &Fake::up(&["a", "c"]));
        assert_eq!(r, Ok("a".into()));
        assert_eq!(
            log,
            [
                "abc: a answers ping, using a",
                "abc: b not tried: a was chosen first",
                "abc: c not tried: it is listed after b, whose reachability_check is off",
            ]
        );
        // a down: b is taken unpinged, and c was beaten by it.
        let (r, log, _) = timed(&yaml, "abc", &Fake::up(&["c"]));
        assert_eq!(r, Ok("b".into()));
        assert_eq!(log[0], "abc: a does not answer ping within 500ms");
        assert!(
            log[1].starts_with("abc: using b (reachability_check is off, "),
            "{log:?}"
        );
        assert_eq!(log[2], "abc: c not tried: b was chosen first");
        assert_eq!(log.len(), 3, "{log:?}");
    }

    #[test]
    fn nothing_answering_tries_every_entry_and_adds_no_line() {
        // The failure path is unchanged: each entry is reached, so each
        // already says why, and the error names them all.
        let (r, log, _) = timed(ABC, "abc", &Fake::new(&[]));
        assert_eq!(
            r,
            Err("no host for 'abc' is reachable (tried a, b, c)".into())
        );
        assert_eq!(
            log,
            [
                "abc: a does not answer ping within 500ms",
                "abc: b does not answer ping within 500ms",
                "abc: c does not answer ping within 500ms",
            ]
        );
    }

    // ---- prefer_local_network, host-side (acs-sia) -------------------------

    /// devbox.example.com first, devbox.lan second, as a home alias lists
    /// them; `extra` goes into the alias's settings.
    fn home(extra: &str) -> String {
        format!(
            "aliases:\n  devbox:\n{extra}    hosts:\n      - host: devbox.example.com\n      - host: devbox.lan\n"
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
                "devbox: devbox.example.com not tried: devbox.lan was chosen first",
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
        let yaml = "aliases:\n  devbox:\n    prefer_local_network: true\n    hosts:\n      - host: devbox.example.com\n        prefer: true\n      - host: devbox.lan\n";
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

    // ---- local_networks, caller-side (acs-9yv) -----------------------------

    /// The bead's example: the target at 172.16.8.2, this machine on
    /// 172.16.1.65 -- the same site, a different /24 -- and an IPv6 pair
    /// beside it, likewise a /64 apart.
    const SITE_NAMES: &[(&str, &str)] = &[
        ("devbox.lan", "172.16.8.2"),
        ("devbox.lan", "2001:db8:1:8::2"),
        ("devbox.example.com", "203.0.113.9"),
    ];

    /// This machine at the site, on its own /24 and /64.
    fn at_site() -> Network {
        on(&["172.16.1.65/24", "2001:db8:1:1::65/64"], SITE_NAMES, 0)
    }

    fn both_up() -> Arc<Fake> {
        Fake::up(&["devbox.example.com", "devbox.lan"])
    }

    /// devbox.example.com first, then devbox.lan carrying `nets` as its
    /// own `local_networks`.
    fn lan_on(nets: &str) -> String {
        format!(
            "aliases:\n  devbox:\n    hosts:\n      - host: devbox.example.com\n      \
             - host: devbox.lan\n        local_networks: {nets}\n"
        )
    }

    #[test]
    fn an_entry_is_local_when_this_machine_is_on_one_of_its_networks() {
        // The interfaces' own prefixes are too narrow to reach the target,
        // and nothing else says the site is local: configured order.
        let (r, log) = located(&lan_on("[10.0.0.0/8]"), &both_up(), at_site());
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert!(
            !log.iter().any(|l| l.contains("this machine is on")),
            "{log:?}"
        );
        // This machine at 172.16.1.65 is inside the entry's /16, so the
        // entry is the local one although it is listed second.
        let (r, log) = located(&lan_on("[172.16.0.0/16]"), &both_up(), at_site());
        assert_eq!(r, Ok("devbox.lan".into()));
        assert_eq!(
            log,
            [
                "devbox: this machine is on 172.16.0.0/16, so devbox.lan is local",
                "devbox: devbox.lan answers ping, using devbox.lan (this machine is on 172.16.0.0/16)",
                "devbox: devbox.example.com not tried: devbox.lan was chosen first",
            ]
        );
        // IPv6 the same way, matched against this machine's 2001:db8:1:1::65.
        let (r, log) = located(&lan_on("[2001:db8:1::/48]"), &both_up(), at_site());
        assert_eq!(r, Ok("devbox.lan".into()));
        assert_eq!(
            log[0],
            "devbox: this machine is on 2001:db8:1::/48, so devbox.lan is local"
        );
        // The first of the entry's own networks this machine is on is the
        // one named.
        let (r, log) = located(
            &lan_on("[10.0.0.0/8, 172.16.0.0/16, 2001:db8:1::/48]"),
            &both_up(),
            at_site(),
        );
        assert_eq!(r, Ok("devbox.lan".into()));
        assert_eq!(
            log[0],
            "devbox: this machine is on 172.16.0.0/16, so devbox.lan is local"
        );
    }

    /// The regression test for acs-9yv: the address tested is the
    /// **caller's**, not the host's. As acs-c9d built it, a host inside the
    /// configured network was ranked first wherever the caller was; here
    /// the host is inside it and the caller is not, so it is not.
    #[test]
    fn a_host_inside_the_network_is_not_local_when_the_caller_is_elsewhere() {
        let yaml = lan_on("[172.16.0.0/16]");
        // devbox.lan resolves to 172.16.8.2, inside the entry's /16 --
        // which is exactly what the old, target-side test matched on. From
        // a coffee shop on 10.0.0.0/8 it must not rank first.
        let (r, log) = located(&yaml, &both_up(), on(&["10.0.0.5/8"], SITE_NAMES, 0));
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert!(!log.iter().any(|l| l.contains("local")), "{log:?}");
        // Nor when the machine has no usable network at all.
        let (r, log) = located(&yaml, &both_up(), on(&[], SITE_NAMES, 0));
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert!(!log.iter().any(|l| l.contains("local")), "{log:?}");
        // Back at the site, the same configuration ranks it first.
        let (r, _) = located(&yaml, &both_up(), at_site());
        assert_eq!(r, Ok("devbox.lan".into()));
    }

    #[test]
    fn a_caller_side_match_needs_no_resolver_and_no_prefer_local_network() {
        // No `prefer_local_network`, and a resolver that would hang past
        // the deadline: the caller-side rank is known without it, so the
        // choice is immediate and right.
        let yaml = lan_on("[172.16.0.0/16]");
        let start = Instant::now();
        let (r, _) = located(
            &yaml,
            &both_up(),
            on(&["172.16.1.65/24"], SITE_NAMES, 10_000),
        );
        assert_eq!(r, Ok("devbox.lan".into()));
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "{:?}",
            start.elapsed()
        );
        // With no entry carrying the setting and `prefer_local_network`
        // off, the network is never asked for at all.
        let c = config(&home(""));
        let r = resolve(
            "devbox",
            &c,
            both_up().reachable(),
            &|| panic!("the network was asked for"),
            &mut |_| {},
        );
        assert_eq!(r.unwrap().unwrap().destination(), "devbox.example.com");
    }

    #[test]
    fn a_local_entry_must_still_answer_and_beats_a_preferred_one() {
        // Local but not answering: the next in rank.
        let fake = Fake::up(&["devbox.example.com"]);
        let (r, log) = located(&lan_on("[172.16.0.0/16]"), &fake, at_site());
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert_eq!(
            log[1],
            "devbox: devbox.lan does not answer ping within 500ms"
        );
        // prefer: true on the other host: the local entry still wins.
        let yaml = "aliases:\n  devbox:\n    hosts:\n      - host: devbox.example.com\n        \
                    prefer: true\n      - host: devbox.lan\n        \
                    local_networks: [172.16.0.0/16]\n";
        let (r, _) = located(yaml, &both_up(), at_site());
        assert_eq!(r, Ok("devbox.lan".into()));
    }

    #[test]
    fn the_two_directions_sit_side_by_side_and_the_host_side_is_named_first() {
        // devbox.lan is local to the caller by its own setting;
        // devbox.example.com is on one of this machine's interfaces'
        // networks. Both are local, so configured order decides between
        // them -- and each is named the way it matched.
        let yaml = "aliases:\n  devbox:\n    prefer_local_network: true\n    hosts:\n      \
                    - host: devbox.example.com\n      - host: devbox.lan\n        \
                    local_networks: [172.16.0.0/16]\n";
        let names = &[
            ("devbox.lan", "172.16.8.2"),
            ("devbox.example.com", "192.168.1.9"),
        ];
        let (r, log) = located(
            yaml,
            &both_up(),
            on(&["192.168.1.5/24", "172.16.1.65/24"], names, 0),
        );
        assert_eq!(r, Ok("devbox.example.com".into()));
        assert_eq!(
            log[..2],
            [
                "devbox: devbox.example.com is on the local network 192.168.1.0/24",
                "devbox: this machine is on 172.16.0.0/16, so devbox.lan is local",
            ]
        );
        // Where one entry matches both ways, the host's own network is the
        // statement named: devbox.lan resolves onto this machine's /24 and
        // its own setting also holds.
        let yaml = "aliases:\n  devbox:\n    prefer_local_network: true\n    hosts:\n      \
                    - host: devbox.example.com\n      - host: devbox.lan\n        \
                    local_networks: [192.168.0.0/16]\n";
        let (r, log) = located(yaml, &both_up(), on(&["192.168.1.5/24"], HOME_NAMES, 0));
        assert_eq!(r, Ok("devbox.lan".into()));
        assert_eq!(
            log[0],
            "devbox: devbox.lan is on the local network 192.168.1.0/24"
        );
    }

    #[test]
    fn the_rank_is_local_network_then_prefer_then_order() {
        let c = config(
            "aliases:\n  x:\n    - host: a\n    - host: b\n      prefer: true\n    - host: c\n    - host: d\n",
        );
        let entries = &c.alias("x").unwrap().entries;
        let n = LocalNet::new("192.168.1.5".parse().unwrap(), 24);
        let (host, caller) = (Some(Local::Host(n)), Some(Local::Caller(n)));
        assert_eq!(rank(entries, &[]), [1, 0, 2, 3]);
        assert_eq!(rank(entries, &[None, None, None, host]), [3, 1, 0, 2]);
        assert_eq!(rank(entries, &[None, host, host, None]), [1, 2, 0, 3]);
        // Either direction makes an entry local, and the two rank alike:
        // among the local ones, configured order decides (acs-9yv).
        assert_eq!(rank(entries, &[None, None, None, caller]), [3, 1, 0, 2]);
        assert_eq!(rank(entries, &[caller, None, host, None]), [0, 2, 1, 3]);
    }

    #[test]
    fn the_aliass_own_deadline_is_the_one_used() {
        let yaml = "\
reachability_timeout: 2s
aliases:
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
        for (ms, secs) in [(1500, 3), (2500, 4), (4000, 6)] {
            assert_eq!(
                ping_with(p.as_os_str(), "h.lan", Duration::from_millis(ms)),
                Some(0)
            );
            assert_eq!(
                std::fs::read_to_string(&args).unwrap(),
                format!("-c 1 {flag} {secs} -- h.lan\n")
            );
        }
        let down = script(&dir, "down", "exit 1");
        assert_eq!(
            ping_with(down.as_os_str(), "h", Duration::from_secs(1)),
            Some(1)
        );
        let none = dir.path().join("no-such-ping");
        assert_eq!(
            ping_with(none.as_os_str(), "h", Duration::from_secs(1)),
            None
        );
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
        assert_eq!(
            ping_with(p.as_os_str(), "h", Duration::from_millis(100)),
            None
        );
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
