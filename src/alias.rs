//! Host aliases (DESIGN §7.3): `acs <alias>` connects to the first of the
//! alias's configured hosts that answers a ping, or that is not checked.

use std::process::{Command, Stdio};

use crate::config::{Config, HostEntry};

/// The ssh destination `name` stands for: `Ok(None)` if it is not an alias
/// (or has a `user@` part), the chosen entry's destination otherwise, and an
/// error naming every host tried when none answers.
///
/// `reachable` pings one host; `log` receives why each entry was taken or
/// skipped.
pub fn resolve(
    name: &str,
    config: &Config,
    reachable: &mut dyn FnMut(&str) -> bool,
    log: &mut dyn FnMut(String),
) -> Result<Option<String>, String> {
    if name.contains('@') {
        return Ok(None);
    }
    let Some(entries) = config.alias(name) else {
        return Ok(None);
    };
    pick(name, entries, reachable, log).map(Some)
}

fn pick(
    name: &str,
    entries: &[HostEntry],
    reachable: &mut dyn FnMut(&str) -> bool,
    log: &mut dyn FnMut(String),
) -> Result<String, String> {
    let mut tried = Vec::new();
    for e in entries {
        let dest = e.destination();
        if !e.reachability_check {
            log(format!(
                "{name}: using {dest} (reachability_check is off, {})",
                e.origin
            ));
            return Ok(dest);
        }
        if reachable(&e.host) {
            log(format!("{name}: {} answers ping, using {dest}", e.host));
            return Ok(dest);
        }
        log(format!("{name}: {} does not answer ping", e.host));
        tried.push(e.host.as_str());
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
    // Spawned apart from waited for: `acs --list` pings from several threads.
    crate::sys::spawn(&mut c)
        .and_then(|mut child| child.wait())
        .map(|s| s.success())
        .unwrap_or(false)
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

    /// Resolve with `up` as the hosts that answer; returns the result, the
    /// hosts pinged, and the log.
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
        (r, pinged, log)
    }

    #[test]
    fn a_name_that_is_not_an_alias_is_left_alone() {
        let (r, pinged, _) = run("other", &[]);
        assert_eq!(r, Ok(None));
        assert!(pinged.is_empty());
        // A user@ part means the name is a host, even if it is an alias.
        let (r, pinged, _) = run("me@devbox", &[]);
        assert_eq!(r, Ok(None));
        assert!(pinged.is_empty());
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
