//! Host aliases (DESIGN §7.3): `acs [user@]<alias>` connects to the first of
//! the alias's configured hosts that answers a ping, or that is not checked.

use std::process::{Command, Stdio};

use crate::config::{Config, HostEntry};

/// The entry `name` stands for: `Ok(None)` if it is not an alias, the chosen
/// entry otherwise, and an error naming every host tried when none answers.
///
/// `user@<alias>` goes through the alias as that user: the chosen entry's
/// `user` is the given one, whatever the entry says. `reachable` pings one
/// host; `log` receives why each entry was taken or skipped.
pub fn resolve(
    name: &str,
    config: &Config,
    reachable: &mut dyn FnMut(&str) -> bool,
    log: &mut dyn FnMut(String),
) -> Result<Option<HostEntry>, String> {
    let (user, alias) = split_user(name);
    let Some(entries) = config.alias(alias) else {
        return Ok(None);
    };
    if let Some(u) = user {
        log(format!("{name}: logging in as {u}, from the command line"));
    }
    pick(name, entries, user, reachable, log).map(Some)
}

/// `user@host` as its login name and host, split at the last `@` as ssh
/// does; a name without one (or with an empty user) has no login name.
pub fn split_user(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        _ => (None, name),
    }
}

fn pick(
    name: &str,
    entries: &[HostEntry],
    user: Option<&str>,
    reachable: &mut dyn FnMut(&str) -> bool,
    log: &mut dyn FnMut(String),
) -> Result<HostEntry, String> {
    let mut tried = Vec::new();
    for e in entries {
        let e = HostEntry {
            user: user.map(String::from).or_else(|| e.user.clone()),
            ..e.clone()
        };
        let dest = e.destination();
        if !e.reachability_check {
            log(format!(
                "{name}: using {dest} (reachability_check is off, {})",
                e.origin
            ));
            return Ok(e);
        }
        if reachable(&e.host) {
            log(format!("{name}: {} answers ping, using {dest}", e.host));
            return Ok(e);
        }
        log(format!("{name}: {} does not answer ping", e.host));
        tried.push(e.host);
    }
    Err(format!(
        "no host for '{name}' is reachable (tried {})",
        tried.join(", ")
    ))
}

/// Ping `host` once with a short timeout (`ACS_PING` names the program).
pub fn ping(host: &str) -> bool {
    let prog = std::env::var_os("ACS_PING")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "ping".into());
    let mut c = Command::new(prog);
    // One packet, give up after 2 s: macOS spells the deadline -t, Linux
    // (iputils and busybox) -W.
    let timeout = if cfg!(target_os = "macos") {
        "-t"
    } else {
        "-W"
    };
    c.args(["-c", "1", timeout, "2", "--", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c.status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

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

    /// Resolve with `up` as the hosts that answer; returns the chosen
    /// destination, the hosts pinged, and the log.
    fn run(name: &str, up: &[&str]) -> (Result<Option<String>, String>, Vec<String>, Vec<String>) {
        let c = config(TWO);
        let mut pinged = Vec::new();
        let mut log = Vec::new();
        let r = resolve(
            name,
            &c,
            &mut |h| {
                pinged.push(h.to_string());
                up.contains(&h)
            },
            &mut |m| log.push(m),
        );
        (r.map(|e| e.map(|e| e.destination())), pinged, log)
    }

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
        assert_eq!(pinged, ["devbox.lan"]);
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
        assert_eq!(pinged, ["devbox.lan", "devbox.example.com"]);
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
        assert_eq!(pinged, ["devbox.lan"]);
        assert_eq!(log, ["devbox: devbox.lan answers ping, using devbox.lan"]);
    }

    #[test]
    fn falls_back_to_the_next_host_with_its_user() {
        let (r, pinged, log) = run("devbox", &["devbox.example.com"]);
        assert_eq!(r, Ok(Some("me@devbox.example.com".into())));
        assert_eq!(pinged, ["devbox.lan", "devbox.example.com"]);
        assert_eq!(log[0], "devbox: devbox.lan does not answer ping");
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
    fn none_reachable_names_every_host_tried() {
        let (r, pinged, _) = run("devbox", &[]);
        assert_eq!(
            r,
            Err("no host for 'devbox' is reachable (tried devbox.lan, devbox.example.com)".into())
        );
        assert_eq!(pinged.len(), 2);
    }
}
