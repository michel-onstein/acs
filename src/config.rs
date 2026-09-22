//! The client's configuration file (DESIGN §7.2): YAML at
//! `/etc/acs/config.yaml` (global), then `$XDG_CONFIG_HOME/acs/config.yaml`
//! (local, default `~/.config/acs/config.yaml`). Either may be missing.
//!
//! Merging: a setting in the local file replaces the global one (an alias's
//! own settings too); mappings (`aliases`) merge key by key; lists (an
//! alias's hosts) concatenate, global entries first. An empty value (`key:`)
//! sets nothing.
//!
//! Only the local client reads it; the remote roles never do.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::netmatch::LocalNet;
use crate::yaml::{self, Node, Value};

/// Where a value came from: a file and line, for `acs config show` and error
/// messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub file: PathBuf,
    pub line: usize,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file.display(), self.line)
    }
}

/// A setting and where it was set; `origin` is `None` for the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub origin: Option<Origin>,
}

impl<T> Setting<T> {
    fn default(value: T) -> Setting<T> {
        Setting {
            value,
            origin: None,
        }
    }
}

/// One way to reach an alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub host: String,
    /// Login name; `None` lets `~/.ssh/config` decide.
    pub user: Option<String>,
    /// Ping the host once before using it (default true).
    pub reachability_check: bool,
    /// Chosen over the alias's other hosts when several answer (DESIGN
    /// §7.3); among the preferred, and among the rest, order decides.
    pub prefer: bool,
    /// The ssh key for this host (`-i`); `None` leaves it to the alias.
    pub identity_file: Option<Setting<String>>,
    /// Keep waiting for the host when it is lost (DESIGN §5.3); `None`
    /// leaves it to the alias.
    pub persist: Option<Setting<bool>>,
    /// Networks that make this entry the local one (acs-9yv): it is ranked
    /// first when one of **this machine's own** addresses is on one of
    /// them. Empty for none.
    pub local_networks: Vec<LocalNet>,
    pub origin: Origin,
}

impl HostEntry {
    /// The ssh destination: `user@host`, or `host`.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// An alias (DESIGN §7.3): its entries in the order they are tried, and
/// what they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub name: String,
    pub entries: Vec<HostEntry>,
    /// The ssh key of every entry that names none itself.
    pub identity_file: Option<Setting<String>>,
    /// Send Ctrl-L after reconnecting; `None` leaves it to the global
    /// setting.
    pub redraw_on_reconnect: Option<Setting<bool>>,
    /// How long to wait for the hosts' pings; `None` leaves it to the
    /// global setting.
    pub reachability_timeout: Option<Setting<Duration>>,
    /// Keep waiting for a lost host; `None` leaves it to the global
    /// setting.
    pub persist: Option<Setting<bool>>,
    /// Try first the hosts on a network this machine is on; `None` leaves
    /// it to the global setting.
    pub prefer_local_network: Option<Setting<bool>>,
    /// How often to ping a lost host while waiting; `None` leaves it to the
    /// global setting.
    pub reachability_interval: Option<Setting<Duration>>,
    /// Where the alias is first defined.
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Install acs on a remote that lacks it (default true).
    pub install_on_remote: Setting<bool>,
    /// Check GitHub once a week for a newer release (default true).
    pub update_check: Setting<bool>,
    /// Ring the terminal bell when command mode arms (default true).
    pub command_bell: Setting<bool>,
    /// Send Ctrl-L to the program after reconnecting to a session that was
    /// already there (default true; DESIGN §5.2).
    pub redraw_on_reconnect: Setting<bool>,
    /// How long an alias's hosts have to answer a ping (default 0.5 s;
    /// DESIGN §7.3).
    pub reachability_timeout: Setting<Duration>,
    /// Keep waiting for a lost host, dialling once it answers a ping
    /// (default false; DESIGN §5.3).
    pub persist: Setting<bool>,
    /// Try an alias's hosts on a network this machine is on first (default
    /// false; DESIGN §7.3).
    pub prefer_local_network: Setting<bool>,
    /// How often a lost host is pinged while waiting (default 5 s).
    pub reachability_interval: Setting<Duration>,
    /// Aliases in the order first defined, each with its hosts in order.
    pub hosts: Vec<Alias>,
    /// The files that were read, global first.
    pub files: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            install_on_remote: Setting::default(true),
            update_check: Setting::default(true),
            command_bell: Setting::default(true),
            redraw_on_reconnect: Setting::default(true),
            reachability_timeout: Setting::default(DEFAULT_REACHABILITY_TIMEOUT),
            persist: Setting::default(false),
            prefer_local_network: Setting::default(false),
            reachability_interval: Setting::default(DEFAULT_REACHABILITY_INTERVAL),
            hosts: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// Every top-level setting, for error messages and `acs config`.
pub const KEYS: &[&str] = &[
    "install_on_remote",
    "update_check",
    "command_bell",
    "redraw_on_reconnect",
    "reachability_timeout",
    "persist",
    "reachability_interval",
    "prefer_local_network",
    "aliases",
];

/// The settings that are true or false.
pub const BOOLS: &[&str] = &[
    "install_on_remote",
    "update_check",
    "command_bell",
    "redraw_on_reconnect",
    "persist",
    "prefer_local_network",
];

/// Every key of a host entry.
pub const HOST_KEYS: &[&str] = &[
    "host",
    "user",
    "reachability_check",
    "identity_file",
    "persist",
    "prefer",
    "local_networks",
];

/// Every key of an alias written as a mapping (`devbox: {identity_file: …,
/// hosts: […]}`) rather than as its list of hosts.
pub const ALIAS_KEYS: &[&str] = &[
    "identity_file",
    "redraw_on_reconnect",
    "reachability_timeout",
    "persist",
    "reachability_interval",
    "prefer_local_network",
    "hosts",
];

/// The alias settings that are true or false.
pub const ALIAS_BOOLS: &[&str] = &["redraw_on_reconnect", "persist", "prefer_local_network"];

/// The settings, global or an alias's, that are a duration.
pub const DURATIONS: &[&str] = &["reachability_timeout", "reachability_interval"];

/// `reachability_timeout` when nothing sets it.
pub const DEFAULT_REACHABILITY_TIMEOUT: Duration = Duration::from_millis(500);

/// The longest `reachability_timeout`: a minute is already far past any
/// ping worth waiting for.
pub const MAX_REACHABILITY_TIMEOUT: Duration = Duration::from_secs(60);

/// `reachability_interval` when nothing sets it.
pub const DEFAULT_REACHABILITY_INTERVAL: Duration = Duration::from_secs(5);

/// The bounds of `reachability_interval`: often enough to notice a host
/// within the hour, not so often that the pings flood it.
pub const MIN_REACHABILITY_INTERVAL: Duration = Duration::from_millis(100);
pub const MAX_REACHABILITY_INTERVAL: Duration = Duration::from_secs(3600);

/// The global file: `/etc/acs/config.yaml` (`ACS_GLOBAL_CONFIG` overrides it,
/// for packagers and tests).
pub fn global_path() -> PathBuf {
    std::env::var_os("ACS_GLOBAL_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/acs/config.yaml"))
}

/// The local file: `$XDG_CONFIG_HOME/acs/config.yaml`, or
/// `~/.config/acs/config.yaml` when that is unset or relative (as the XDG
/// Base Directory spec says).
pub fn local_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    base.join("acs/config.yaml")
}

/// Read a file into a document; `Ok(None)` if it does not exist.
pub fn read_doc(path: &Path) -> Result<Option<yaml::Document>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    yaml::parse(&text)
        .map(Some)
        .map_err(|e| format!("{}:{}: {}", path.display(), e.line, e.msg))
}

impl Config {
    /// The global then the local file.
    pub fn load() -> Result<Config, String> {
        Config::load_files(&[global_path(), local_path()])
    }

    /// Read `files` in order; later ones override earlier ones.
    pub fn load_files(files: &[PathBuf]) -> Result<Config, String> {
        let mut c = Config::default();
        for f in files {
            if let Some(doc) = read_doc(f)? {
                c.apply(f, &doc.root)?;
                c.files.push(f.clone());
            }
        }
        c.check()?;
        Ok(c)
    }

    /// What only the merged files can get wrong: one file may set an
    /// alias's identity_file over another's hosts, but together they must
    /// name a host.
    pub fn check(&self) -> Result<(), String> {
        match self.hosts.iter().find(|a| a.entries.is_empty()) {
            Some(a) => Err(format!("{}: aliases.{}: no hosts listed", a.origin, a.name)),
            None => Ok(()),
        }
    }

    /// Merge one file's settings over what is there.
    pub fn apply(&mut self, file: &Path, root: &Node) -> Result<(), String> {
        let at = |n: &Node| Origin {
            file: file.to_path_buf(),
            line: n.line,
        };
        let fail = |n: &Node, msg: String| format!("{}: {msg}", at(n));
        let entries = match &root.value {
            Value::Null => return Ok(()),
            Value::Map(m) => m,
            other => {
                return Err(fail(
                    root,
                    format!(
                        "expected settings as 'key: value' lines, found {}",
                        other.kind()
                    ),
                ))
            }
        };
        for (key, node) in entries {
            if node.value == Value::Null {
                continue;
            }
            match key.as_str() {
                k if BOOLS.contains(&k) => {
                    let value = bool_value(node).map_err(|e| fail(node, format!("{key}: {e}")))?;
                    *self.bool_mut(k).expect("a bool setting") = Setting {
                        value,
                        origin: Some(at(node)),
                    }
                }
                k if DURATIONS.contains(&k) => {
                    let value =
                        duration_value(k, node).map_err(|e| fail(node, format!("{key}: {e}")))?;
                    *self.duration_mut(k).expect("a duration setting") = Setting {
                        value,
                        origin: Some(at(node)),
                    }
                }
                "aliases" => {
                    let aliases = node.value.map().ok_or_else(|| {
                        fail(
                            node,
                            format!(
                                "aliases: expected a mapping of alias names, found {}",
                                node.value.kind()
                            ),
                        )
                    })?;
                    for (name, n) in aliases {
                        validate_alias(name).map_err(|e| fail(n, e))?;
                        let (entries, settings) = alias_parts(name, n, &at)?;
                        let i = match self.hosts.iter().position(|a| a.name == *name) {
                            Some(i) => i,
                            None => {
                                self.hosts.push(Alias {
                                    name: name.clone(),
                                    entries: Vec::new(),
                                    identity_file: None,
                                    redraw_on_reconnect: None,
                                    reachability_timeout: None,
                                    persist: None,
                                    prefer_local_network: None,
                                    reachability_interval: None,
                                    origin: at(n),
                                });
                                self.hosts.len() - 1
                            }
                        };
                        let alias = &mut self.hosts[i];
                        alias.entries.extend(entries);
                        settings.merge_into(alias);
                    }
                }
                // Renamed (acs-msy): say so rather than call it unknown.
                "hosts" => {
                    return Err(fail(
                        node,
                        "'hosts' is now 'aliases': rename the key".into(),
                    ))
                }
                other => {
                    return Err(fail(
                        node,
                        format!("unknown setting '{other}' (known: {})", KEYS.join(", ")),
                    ))
                }
            }
        }
        Ok(())
    }

    /// A true-or-false setting by name.
    pub fn bool_setting(&self, key: &str) -> Option<&Setting<bool>> {
        match key {
            "install_on_remote" => Some(&self.install_on_remote),
            "update_check" => Some(&self.update_check),
            "command_bell" => Some(&self.command_bell),
            "redraw_on_reconnect" => Some(&self.redraw_on_reconnect),
            "persist" => Some(&self.persist),
            "prefer_local_network" => Some(&self.prefer_local_network),
            _ => None,
        }
    }

    fn bool_mut(&mut self, key: &str) -> Option<&mut Setting<bool>> {
        match key {
            "install_on_remote" => Some(&mut self.install_on_remote),
            "update_check" => Some(&mut self.update_check),
            "command_bell" => Some(&mut self.command_bell),
            "redraw_on_reconnect" => Some(&mut self.redraw_on_reconnect),
            "persist" => Some(&mut self.persist),
            "prefer_local_network" => Some(&mut self.prefer_local_network),
            _ => None,
        }
    }

    /// A duration setting by name.
    pub fn duration_setting(&self, key: &str) -> Option<&Setting<Duration>> {
        match key {
            "reachability_timeout" => Some(&self.reachability_timeout),
            "reachability_interval" => Some(&self.reachability_interval),
            _ => None,
        }
    }

    fn duration_mut(&mut self, key: &str) -> Option<&mut Setting<Duration>> {
        match key {
            "reachability_timeout" => Some(&mut self.reachability_timeout),
            "reachability_interval" => Some(&mut self.reachability_interval),
            _ => None,
        }
    }

    /// The alias called `name`, if there is one.
    pub fn alias(&self, name: &str) -> Option<&Alias> {
        self.hosts.iter().find(|a| a.name == name)
    }

    /// `redraw_on_reconnect` for a session reached as `name` (`[user@]<alias>`,
    /// or `None` for a plain host): the alias's own setting, else the global
    /// one.
    pub fn redraw_on_reconnect_for(&self, name: Option<&str>) -> &Setting<bool> {
        name.and_then(|n| self.alias(crate::alias::split_user(n).1))
            .and_then(|a| a.redraw_on_reconnect.as_ref())
            .unwrap_or(&self.redraw_on_reconnect)
    }

    /// `reachability_timeout` for `alias`: its own setting, else the global
    /// one.
    pub fn reachability_timeout_for<'a>(&'a self, alias: &'a Alias) -> &'a Setting<Duration> {
        alias
            .reachability_timeout
            .as_ref()
            .unwrap_or(&self.reachability_timeout)
    }

    /// `reachability_interval` for a host reached through `alias` (or
    /// none): the alias's own setting, else the global one.
    pub fn reachability_interval_for<'a>(
        &'a self,
        alias: Option<&'a Alias>,
    ) -> &'a Setting<Duration> {
        alias
            .and_then(|a| a.reachability_interval.as_ref())
            .unwrap_or(&self.reachability_interval)
    }

    /// `persist` for a host reached through `alias` as its `entry`: the
    /// entry's own setting, else the alias's, else the global one (DESIGN
    /// §5.3). Without an entry in use (none of the alias's hosts answered)
    /// the alias decides.
    pub fn persist_for<'a>(
        &'a self,
        alias: Option<&'a Alias>,
        entry: Option<&'a HostEntry>,
    ) -> &'a Setting<bool> {
        entry
            .and_then(|e| e.persist.as_ref())
            .or_else(|| alias.and_then(|a| a.persist.as_ref()))
            .unwrap_or(&self.persist)
    }

    /// `prefer_local_network` for `alias`: its own setting, else the global
    /// one.
    pub fn prefer_local_network_for<'a>(&'a self, alias: &'a Alias) -> &'a Setting<bool> {
        alias
            .prefer_local_network
            .as_ref()
            .unwrap_or(&self.prefer_local_network)
    }
}

/// An alias is used where a host name goes: no `@`, spaces or leading `-`.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty()
        || alias.starts_with('-')
        || alias.contains('@')
        || alias.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "bad alias name '{alias}' (no '@', spaces or leading '-')"
        ));
    }
    Ok(())
}

/// `true` or `false` (YAML 1.2, any case).
pub fn bool_value(n: &Node) -> Result<bool, String> {
    match &n.value {
        Value::Scalar(s) if s.style == yaml::Style::Plain => {
            match s.text.to_ascii_lowercase().as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(format!("expected true or false, found '{}'", s.text)),
            }
        }
        Value::Scalar(s) => Err(format!(
            "expected true or false, found the string \"{}\"",
            s.text
        )),
        other => Err(format!("expected true or false, found {}", other.kind())),
    }
}

/// A duration setting's node: see [`parse_duration`].
fn duration_value(key: &str, n: &Node) -> Result<Duration, String> {
    match &n.value {
        Value::Scalar(s) => parse_duration(key, &s.text),
        other => Err(format!(
            "expected a duration such as 500ms or 2s, found {}",
            other.kind()
        )),
    }
}

/// A `reachability_timeout`: see [`parse_duration`].
pub fn parse_timeout(text: &str) -> Result<Duration, String> {
    parse_duration("reachability_timeout", text)
}

/// The least and the most each duration setting takes.
fn duration_bounds(key: &str) -> (Duration, Duration) {
    match key {
        "reachability_interval" => (MIN_REACHABILITY_INTERVAL, MAX_REACHABILITY_INTERVAL),
        _ => (Duration::from_millis(1), MAX_REACHABILITY_TIMEOUT),
    }
}

/// Duration setting `key` as `500ms`, `0.5s`, `2s`, or a bare number of
/// seconds (`0.5`); to the millisecond, within the setting's bounds
/// (`reachability_timeout`: 1 ms to a minute; `reachability_interval`:
/// 100 ms to an hour).
pub fn parse_duration(key: &str, text: &str) -> Result<Duration, String> {
    let (min, max) = duration_bounds(key);
    let bad = || format!("expected a duration such as 500ms or 2s, found '{text}'");
    let t = text.trim();
    let (number, scale) = match t.strip_suffix("ms") {
        Some(n) => (n, 1.0),
        None => (t.strip_suffix('s').unwrap_or(t), 1000.0),
    };
    let number = number.trim_end();
    // Decimal figures and at most one point: no sign, exponent, inf or nan.
    if number.is_empty()
        || number == "."
        || !number.chars().all(|c| c.is_ascii_digit() || c == '.')
        || number.matches('.').count() > 1
    {
        return Err(bad());
    }
    let ms = (number.parse::<f64>().map_err(|_| bad())? * scale).round();
    let d = Duration::from_millis(ms.clamp(0.0, u64::MAX as f64) as u64);
    if d < min {
        return Err(format!(
            "'{text}' is too short: at least {}",
            format_timeout(min)
        ));
    }
    if d > max {
        return Err(format!(
            "'{text}' is too long: at most {}",
            format_timeout(max)
        ));
    }
    Ok(d)
}

/// A duration as [`parse_timeout`] reads it: `2s` if whole seconds, else
/// `500ms`.
pub fn format_timeout(d: Duration) -> String {
    let ms = d.as_millis();
    if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// A host entry's `local_networks` (acs-9yv): a list of CIDR networks, or
/// one value holding them separated by commas (as `acs config host add
/// --local-networks` takes them).
fn networks_value(n: &Node) -> Result<Vec<LocalNet>, String> {
    match &n.value {
        Value::Seq(items) => items
            .iter()
            .map(|i| match &i.value {
                Value::Scalar(s) => LocalNet::parse(&s.text),
                other => Err(format!(
                    "expected a network such as 172.16.0.0/16, found {}",
                    other.kind()
                )),
            })
            .collect(),
        Value::Scalar(s) => parse_networks(&s.text),
        other => Err(format!(
            "expected a list of networks such as [172.16.0.0/16, fd00::/48], found {}",
            other.kind()
        )),
    }
}

/// Networks separated by commas, as `acs config host add --local-networks`
/// takes them; no network at all is an empty list.
pub fn parse_networks(text: &str) -> Result<Vec<LocalNet>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(LocalNet::parse)
        .collect()
}

fn string_value(n: &Node) -> Result<String, String> {
    match &n.value {
        Value::Scalar(s) if !s.text.is_empty() => Ok(s.text.clone()),
        Value::Scalar(_) => Err("expected a name, found an empty string".into()),
        other => Err(format!("expected a name, found {}", other.kind())),
    }
}

/// `~` or `~/…` with `$HOME`, as the shell expands a `-i` typed on the
/// command line; anything else (`~user/…` too) as is, for ssh to expand.
pub fn expand_home(path: &str) -> String {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    match (home, path.strip_prefix('~')) {
        (Some(h), Some(rest)) if rest.is_empty() || rest.starts_with('/') => format!("{h}{rest}"),
        _ => path.to_string(),
    }
}

/// An `identity_file` value: a path, kept as written (`~` is expanded where
/// it is used); `None` for an empty value.
fn identity_file(
    n: &Node,
    at: &dyn Fn(&Node) -> Origin,
) -> Result<Option<Setting<String>>, String> {
    match &n.value {
        Value::Null => Ok(None),
        Value::Scalar(s) if !s.text.is_empty() && !s.text.chars().any(char::is_control) => {
            Ok(Some(Setting {
                value: s.text.clone(),
                origin: Some(at(n)),
            }))
        }
        Value::Scalar(_) => Err("expected a file path, found an empty string".into()),
        other => Err(format!("expected a file path, found {}", other.kind())),
    }
}

/// The settings one file gives an alias of its own (DESIGN §7.2); `None`
/// where it gives none.
#[derive(Default)]
struct AliasSettings {
    identity_file: Option<Setting<String>>,
    redraw_on_reconnect: Option<Setting<bool>>,
    reachability_timeout: Option<Setting<Duration>>,
    persist: Option<Setting<bool>>,
    reachability_interval: Option<Setting<Duration>>,
    prefer_local_network: Option<Setting<bool>>,
}

impl AliasSettings {
    fn bool_mut(&mut self, key: &str) -> &mut Option<Setting<bool>> {
        match key {
            "redraw_on_reconnect" => &mut self.redraw_on_reconnect,
            "persist" => &mut self.persist,
            "prefer_local_network" => &mut self.prefer_local_network,
            other => unreachable!("not an alias bool: {other}"),
        }
    }

    fn duration_mut(&mut self, key: &str) -> &mut Option<Setting<Duration>> {
        match key {
            "reachability_timeout" => &mut self.reachability_timeout,
            "reachability_interval" => &mut self.reachability_interval,
            other => unreachable!("not a duration: {other}"),
        }
    }

    /// Set on `alias` what this file sets; the rest stays as it was.
    fn merge_into(self, alias: &mut Alias) {
        fn over<T>(to: &mut Option<T>, from: Option<T>) {
            if from.is_some() {
                *to = from;
            }
        }
        over(&mut alias.identity_file, self.identity_file);
        over(&mut alias.redraw_on_reconnect, self.redraw_on_reconnect);
        over(&mut alias.reachability_timeout, self.reachability_timeout);
        over(&mut alias.persist, self.persist);
        over(&mut alias.prefer_local_network, self.prefer_local_network);
        over(&mut alias.reachability_interval, self.reachability_interval);
    }
}

/// One file's entries and settings for `hosts.<name>`: a list of entries,
/// one entry without the list, or a mapping of the alias's settings and its
/// `hosts` (which another file may supply instead).
fn alias_parts(
    name: &str,
    node: &Node,
    at: &dyn Fn(&Node) -> Origin,
) -> Result<(Vec<HostEntry>, AliasSettings), String> {
    let fail = |n: &Node, msg: String| format!("{}: aliases.{name}: {msg}", at(n));
    let mut settings = AliasSettings::default();
    let list = match &node.value {
        Value::Map(m) if !m.iter().any(|(k, _)| k == "host") => {
            let mut list = None;
            for (k, v) in m {
                match k.as_str() {
                    "hosts" => list = Some(v).filter(|v| v.value != Value::Null),
                    "identity_file" => {
                        settings.identity_file = identity_file(v, at)
                            .map_err(|e| fail(v, format!("identity_file: {e}")))?
                    }
                    k if (ALIAS_BOOLS.contains(&k) || DURATIONS.contains(&k))
                        && v.value == Value::Null => {}
                    k if ALIAS_BOOLS.contains(&k) => {
                        let value = bool_value(v).map_err(|e| fail(v, format!("{k}: {e}")))?;
                        *settings.bool_mut(k) = Some(Setting {
                            value,
                            origin: Some(at(v)),
                        })
                    }
                    k if DURATIONS.contains(&k) => {
                        let value =
                            duration_value(k, v).map_err(|e| fail(v, format!("{k}: {e}")))?;
                        *settings.duration_mut(k) = Some(Setting {
                            value,
                            origin: Some(at(v)),
                        })
                    }
                    other => {
                        return Err(fail(
                            v,
                            format!(
                                "unknown key '{other}' (an alias takes {}; a single host entry needs 'host: <name>')",
                                ALIAS_KEYS.join(", ")
                            ),
                        ))
                    }
                }
            }
            match list {
                Some(l) => l,
                None => return Ok((Vec::new(), settings)),
            }
        }
        _ => node,
    };
    let items: Vec<&Node> = match &list.value {
        Value::Seq(v) => v.iter().collect(),
        Value::Map(_) => vec![list],
        _ => {
            return Err(fail(
                list,
                format!(
                    "expected a list of hosts ('- host: name'), found {}",
                    list.value.kind()
                ),
            ))
        }
    };
    let mut entries = Vec::new();
    for item in items {
        entries.push(host_entry(item, at).map_err(|e| fail(item, e))?);
    }
    if entries.is_empty() {
        return Err(fail(list, "no hosts listed".into()));
    }
    Ok((entries, settings))
}

/// Name the field (and its line) in an error about its value.
fn field<T>(key: &str, n: &Node, r: Result<T, String>) -> Result<T, String> {
    r.map_err(|e| format!("{key}: {e} (line {})", n.line))
}

fn host_entry(item: &Node, at: &dyn Fn(&Node) -> Origin) -> Result<HostEntry, String> {
    let fields = item.value.map().ok_or_else(|| {
        format!(
            "expected a host entry ('host: name', optional 'user', 'reachability_check' and 'identity_file'), found {}",
            item.value.kind()
        )
    })?;
    let mut host = None;
    let mut user = None;
    let mut check = true;
    let mut prefer = false;
    let mut identity = None;
    let mut persist = None;
    let mut networks = Vec::new();
    for (k, v) in fields {
        match k.as_str() {
            "host" => {
                let h = field(k, v, string_value(v))?;
                if h.starts_with('-') || h.contains('@') || h.chars().any(char::is_whitespace) {
                    return Err(format!(
                        "host: bad host name '{h}' (put a login name in 'user')"
                    ));
                }
                host = Some(h);
            }
            "user" => {
                if v.value != Value::Null {
                    let u = field(k, v, string_value(v))?;
                    if u.contains('@') || u.chars().any(char::is_whitespace) {
                        return Err(format!("user: bad login name '{u}'"));
                    }
                    user = Some(u);
                }
            }
            "reachability_check" => {
                if v.value != Value::Null {
                    check = field(k, v, bool_value(v))?;
                }
            }
            "prefer" => {
                if v.value != Value::Null {
                    prefer = field(k, v, bool_value(v))?;
                }
            }
            "identity_file" => identity = field(k, v, identity_file(v, at))?,
            "local_networks" => {
                if v.value != Value::Null {
                    networks = field(k, v, networks_value(v))?;
                }
            }
            "persist" => {
                if v.value != Value::Null {
                    persist = Some(Setting {
                        value: field(k, v, bool_value(v))?,
                        origin: Some(at(v)),
                    });
                }
            }
            other => {
                return Err(format!(
                    "unknown key '{other}' in a host entry (known: {})",
                    HOST_KEYS.join(", ")
                ))
            }
        }
    }
    let host = host.ok_or("a host entry needs 'host: <name>'")?;
    Ok(HostEntry {
        host,
        user,
        reachability_check: check,
        prefer,
        identity_file: identity,
        persist,
        local_networks: networks,
        origin: at(item),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn files(dir: &TempDir, global: &str, local: &str) -> Vec<PathBuf> {
        let g = dir.path().join("global.yaml");
        let l = dir.path().join("local.yaml");
        std::fs::write(&g, global).unwrap();
        std::fs::write(&l, local).unwrap();
        vec![g, l]
    }

    fn load(global: &str, local: &str) -> Result<Config, String> {
        let dir = TempDir::new();
        Config::load_files(&files(&dir, global, local))
    }

    fn hosts(c: &Config, alias: &str) -> Vec<String> {
        c.alias(alias)
            .unwrap()
            .entries
            .iter()
            .map(|e| format!("{}/{}", e.destination(), e.reachability_check))
            .collect()
    }

    #[test]
    fn defaults_without_files() {
        let dir = TempDir::new();
        let c = Config::load_files(&[dir.path().join("none.yaml")]).unwrap();
        assert!(c.install_on_remote.value);
        assert_eq!(c.install_on_remote.origin, None);
        assert!(c.command_bell.value);
        assert!(c.hosts.is_empty());
        assert!(c.files.is_empty());
        assert_eq!(c, Config::default());
    }

    #[test]
    fn local_scalars_override_global_ones() {
        let c = load("install_on_remote: false\n", "install_on_remote: true\n").unwrap();
        assert!(c.install_on_remote.value);
        let o = c.install_on_remote.origin.unwrap();
        assert!(o.file.ends_with("local.yaml") && o.line == 1, "{o}");

        let c = load("install_on_remote: false\n", "# nothing here\n").unwrap();
        assert!(!c.install_on_remote.value);
        assert!(c
            .install_on_remote
            .origin
            .unwrap()
            .file
            .ends_with("global.yaml"));

        // An empty value sets nothing.
        let c = load("install_on_remote: false\n", "install_on_remote:\n").unwrap();
        assert!(!c.install_on_remote.value);
    }

    #[test]
    fn host_maps_merge_and_lists_concatenate() {
        let g = "aliases:\n  devbox:\n    - host: devbox.lan\n  nas:\n    - host: nas.lan\n      reachability_check: false\n";
        let l = "aliases:\n  devbox:\n    - host: devbox.example.com\n      user: me\n  pi:\n    host: pi.lan\n";
        let c = load(g, l).unwrap();
        assert_eq!(
            hosts(&c, "devbox"),
            ["devbox.lan/true", "me@devbox.example.com/true"]
        );
        assert_eq!(hosts(&c, "nas"), ["nas.lan/false"]);
        // A single entry needs no list.
        assert_eq!(hosts(&c, "pi"), ["pi.lan/true"]);
        let order: Vec<&str> = c.hosts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(order, ["devbox", "nas", "pi"]);
        let second = &c.alias("devbox").unwrap().entries[1];
        assert!(second.origin.file.ends_with("local.yaml"));
        assert_eq!(second.origin.line, 3);
    }

    #[test]
    fn errors_name_the_file_and_line() {
        let cases: &[(&str, &str)] = &[
            (
                "install_on_remote: maybe\n",
                ":1: install_on_remote: expected true or false",
            ),
            (
                "install_on_remote: \"true\"\n",
                ":1: install_on_remote: expected true or false, found the string",
            ),
            (
                "x: 1\n",
                ":1: unknown setting 'x' (known: install_on_remote, update_check, command_bell, redraw_on_reconnect, reachability_timeout, persist, reachability_interval, prefer_local_network, aliases)",
            ),
            ("aliases: [a]\n", ":1: aliases: expected a mapping"),
            // acs-9yv: a bad network on a host entry, and the setting's
            // old homes -- global and per alias -- which it no longer has.
            (
                "aliases:\n  d:\n    - host: a\n      local_networks: [172.16.0.0]\n",
                ":3: aliases.d: local_networks: expected a network such as 172.16.0.0/16 or fd00::/48, found '172.16.0.0' (line 4)",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      local_networks: [0.0.0.0/0]\n",
                ":3: aliases.d: local_networks: '0.0.0.0/0': a /0 network is every address (line 4)",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      local_networks: [172.16.0.0/16, fd00::/200]\n",
                ":3: aliases.d: local_networks: 'fd00::/200': /200 is too long for an IPv6 network (at most /128) (line 4)",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      local_networks: {a: b}\n",
                ":3: aliases.d: local_networks: expected a list of networks such as [172.16.0.0/16, fd00::/48], found a mapping (line 4)",
            ),
            (
                "local_networks: [172.16.0.0/16]\n",
                ":1: unknown setting 'local_networks'",
            ),
            (
                "aliases:\n  d:\n    local_networks: [172.16.0.0/16]\n    hosts: [a]\n",
                ":3: aliases.d: unknown key 'local_networks'",
            ),
            // acs-msy: the old name of the key says what it is now.
            (
                "command_bell: false\nhosts:\n  d:\n    - host: a\n",
                ":2: 'hosts' is now 'aliases': rename the key",
            ),
            (
                "aliases:\n  d: foo\n",
                ":2: aliases.d: expected a list of hosts",
            ),
            (
                "aliases:\n  d:\n    - user: me\n",
                ":3: aliases.d: a host entry needs 'host: <name>'",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      port: 22\n",
                ":3: aliases.d: unknown key 'port'",
            ),
            (
                "aliases:\n  d:\n    - host: me@a\n",
                ":3: aliases.d: host: bad host name",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      reachability_check: 1\n",
                "reachability_check: expected true or false, found '1' (line 4)",
            ),
            ("aliases:\n  d: []\n", ":2: aliases.d: no hosts listed"),
            (
                "aliases:\n  a@b:\n    - host: x\n",
                ":2: bad alias name 'a@b'",
            ),
            ("- 1\n", ":1: expected settings as 'key: value' lines"),
            ("a: 1\n  b: 2\n", ":2: unexpected indentation"),
        ];
        for (src, want) in cases {
            let e = load("", src).unwrap_err();
            assert!(e.contains("local.yaml") && e.contains(want), "{src:?}: {e}");
        }
    }

    fn identity(s: &Option<Setting<String>>) -> Option<String> {
        s.as_ref().map(|s| {
            let o = s.origin.as_ref().unwrap();
            let f = o.file.file_name().unwrap().to_string_lossy();
            format!("{}@{f}:{}", s.value, o.line)
        })
    }

    #[test]
    fn identity_files_at_the_alias_and_the_entry() {
        let l = "\
aliases:
  devbox:
    identity_file: ~/.ssh/id_devbox
    hosts:
      - host: devbox.lan
        identity_file: ~/.ssh/id_lan
      - host: devbox.example.com
  nas: {identity_file: /keys/nas, hosts: [{host: nas.lan}]}
  pi:
    host: pi.lan
    identity_file: ~/.ssh/id_pi
";
        let c = load("", l).unwrap();
        let d = c.alias("devbox").unwrap();
        assert_eq!(
            identity(&d.identity_file).as_deref(),
            Some("~/.ssh/id_devbox@local.yaml:3")
        );
        assert_eq!(
            identity(&d.entries[0].identity_file).as_deref(),
            Some("~/.ssh/id_lan@local.yaml:6")
        );
        assert_eq!(d.entries[1].identity_file, None);
        assert_eq!(hosts(&c, "devbox").len(), 2);
        let nas = c.alias("nas").unwrap();
        assert_eq!(nas.identity_file.as_ref().unwrap().value, "/keys/nas");
        assert_eq!(hosts(&c, "nas"), ["nas.lan/true"]);
        // One entry without a list keeps its own key: it is not the alias's.
        let pi = c.alias("pi").unwrap();
        assert_eq!(pi.identity_file, None);
        assert_eq!(
            pi.entries[0].identity_file.as_ref().unwrap().value,
            "~/.ssh/id_pi"
        );
    }

    #[test]
    fn the_local_alias_identity_replaces_the_global_one() {
        let g = "aliases:\n  devbox:\n    identity_file: /etc/key\n    hosts:\n      - host: devbox.lan\n";
        // A file may set only the alias's key, over another file's hosts.
        let l = "aliases:\n  devbox:\n    identity_file: ~/.ssh/mine\n";
        let c = load(g, l).unwrap();
        let d = c.alias("devbox").unwrap();
        assert_eq!(
            identity(&d.identity_file).as_deref(),
            Some("~/.ssh/mine@local.yaml:3")
        );
        assert_eq!(hosts(&c, "devbox"), ["devbox.lan/true"]);
        assert!(d.origin.file.ends_with("global.yaml"));

        // An empty value sets nothing; a list adds hosts and keeps the key.
        let c = load(g, "aliases:\n  devbox:\n    identity_file:\n").unwrap();
        assert_eq!(
            identity(&c.alias("devbox").unwrap().identity_file).as_deref(),
            Some("/etc/key@global.yaml:3")
        );
        let c = load(g, "aliases:\n  devbox:\n    - host: devbox.example.com\n").unwrap();
        assert_eq!(
            c.alias("devbox")
                .unwrap()
                .identity_file
                .as_ref()
                .unwrap()
                .value,
            "/etc/key"
        );
        assert_eq!(hosts(&c, "devbox").len(), 2);
    }

    #[test]
    fn an_alias_needs_a_host_in_some_file() {
        let e = load("", "aliases:\n  devbox:\n    identity_file: ~/.ssh/k\n").unwrap_err();
        assert!(
            e.contains("local.yaml:2: aliases.devbox: no hosts listed"),
            "{e}"
        );
        let e = load(
            "",
            "aliases:\n  devbox:\n    identity_file: k\n    hosts: []\n",
        )
        .unwrap_err();
        assert!(
            e.contains("local.yaml:4: aliases.devbox: no hosts listed"),
            "{e}"
        );
    }

    #[test]
    fn identity_file_errors_name_the_file_and_line() {
        let cases: &[(&str, &str)] = &[
            (
                "aliases:\n  d:\n    identity_file: [a, b]\n    hosts:\n      - host: a\n",
                ":3: aliases.d: identity_file: expected a file path, found a list",
            ),
            (
                "aliases:\n  d:\n    identity_file: \"\"\n",
                ":3: aliases.d: identity_file: expected a file path, found an empty string",
            ),
            (
                "aliases:\n  d:\n    user: me\n",
                ":3: aliases.d: unknown key 'user' (an alias takes identity_file, redraw_on_reconnect, reachability_timeout, persist, reachability_interval, prefer_local_network, hosts; a single host entry needs 'host: <name>')",
            ),
            (
                "aliases:\n  d:\n    - host: a\n      identity_file: {k: v}\n",
                ":3: aliases.d: identity_file: expected a file path, found a mapping (line 4)",
            ),
            (
                "aliases:\n  d:\n    hosts: x\n",
                ":3: aliases.d: expected a list of hosts",
            ),
        ];
        for (src, want) in cases {
            let e = load("", src).unwrap_err();
            assert!(e.contains("local.yaml") && e.contains(want), "{src:?}: {e}");
        }
    }

    #[test]
    fn redraw_on_reconnect_the_alias_over_the_global_over_the_default() {
        let at = |s: &Setting<bool>| {
            let line = s.origin.as_ref().map_or(0, |o| o.line);
            format!("{}@{line}", s.value)
        };
        // Nothing set: on.
        let c = load("", "aliases:\n  d:\n    - host: a\n").unwrap();
        assert_eq!(at(c.redraw_on_reconnect_for(Some("d"))), "true@0");
        assert_eq!(at(c.redraw_on_reconnect_for(None)), "true@0");

        let g = "redraw_on_reconnect: false\naliases:\n  d:\n    - host: a\n  e:\n    - host: b\n";
        let l = "aliases:\n  d:\n    redraw_on_reconnect: true\n";
        let c = load(g, l).unwrap();
        // The alias's own setting wins, as `user@<alias>` too; an alias
        // without one, a plain host and an unknown name take the global one.
        assert_eq!(at(c.redraw_on_reconnect_for(Some("d"))), "true@3");
        assert_eq!(at(c.redraw_on_reconnect_for(Some("me@d"))), "true@3");
        assert_eq!(at(c.redraw_on_reconnect_for(Some("e"))), "false@1");
        assert_eq!(at(c.redraw_on_reconnect_for(None)), "false@1");
        assert_eq!(at(c.redraw_on_reconnect_for(Some("other"))), "false@1");
        let d = c.alias("d").unwrap();
        assert!(d
            .redraw_on_reconnect
            .as_ref()
            .unwrap()
            .origin
            .as_ref()
            .unwrap()
            .file
            .ends_with("local.yaml"));
        assert_eq!(hosts(&c, "d"), ["a/true"]);

        // The local file's alias setting replaces the global file's; an
        // empty value sets nothing.
        let g = "aliases:\n  d:\n    redraw_on_reconnect: false\n    hosts: [{host: a}]\n";
        let c = load(g, "aliases:\n  d:\n    redraw_on_reconnect: true\n").unwrap();
        assert_eq!(at(c.redraw_on_reconnect_for(Some("d"))), "true@3");
        let c = load(g, "aliases:\n  d:\n    redraw_on_reconnect:\n").unwrap();
        assert_eq!(at(c.redraw_on_reconnect_for(Some("d"))), "false@3");

        let e = load(
            "",
            "aliases:\n  d:\n    redraw_on_reconnect: 0\n    hosts: [{host: a}]\n",
        )
        .unwrap_err();
        assert!(
            e.contains(
                "local.yaml:3: aliases.d: redraw_on_reconnect: expected true or false, found '0'"
            ),
            "{e}"
        );
        let e = load("", "redraw_on_reconnect: yes\n").unwrap_err();
        assert!(
            e.contains(":1: redraw_on_reconnect: expected true or false"),
            "{e}"
        );
    }

    #[test]
    fn home_is_expanded_like_the_shell_would() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/.ssh/id"), format!("{home}/.ssh/id"));
        assert_eq!(expand_home("~"), home);
        for p in ["~bob/.ssh/id", "/abs/~/x", "rel/key", "~x"] {
            assert_eq!(expand_home(p), p);
        }
    }

    #[test]
    fn persist_at_every_level_and_reachability_interval() {
        let c = load("", "aliases:\n  a: [{host: a1}]\n").unwrap();
        assert!(!c.persist.value);
        assert_eq!(c.reachability_interval.value, Duration::from_secs(5));
        let c = load(
            "persist: true\nreachability_interval: 10s\n",
            "aliases:\n  a:\n    persist: false\n    reachability_interval: 1s\n    hosts:\n      - host: a1\n        persist: true\n      - host: a2\n",
        )
        .unwrap();
        let a = c.alias("a").unwrap();
        assert!(c.persist_for(Some(a), Some(&a.entries[0])).value);
        assert!(!c.persist_for(Some(a), Some(&a.entries[1])).value);
        assert!(!c.persist_for(Some(a), None).value);
        assert!(c.persist_for(None, None).value);
        assert_eq!(
            c.persist_for(Some(a), Some(&a.entries[0]))
                .origin
                .as_ref()
                .unwrap()
                .line,
            7
        );
        assert_eq!(
            c.reachability_interval_for(Some(a)).value,
            Duration::from_secs(1)
        );
        assert_eq!(
            c.reachability_interval_for(None).value,
            Duration::from_secs(10)
        );
        for (text, why) in [
            (
                "reachability_interval: 50ms\n",
                "'50ms' is too short: at least 100ms",
            ),
            ("reachability_interval: 2h\n", "expected a duration"),
            (
                "reachability_interval: 3601\n",
                "'3601' is too long: at most 3600s",
            ),
            ("persist: yes\n", "persist: expected true or false"),
            (
                "aliases:\n  d:\n    - host: a\n      persist: 1\n",
                "persist: expected true or false, found '1' (line 4)",
            ),
        ] {
            let e = load("", text).unwrap_err();
            assert!(e.contains(why), "{text:?}: {e}");
        }
        assert_eq!(
            parse_duration("reachability_interval", "0.1"),
            Ok(Duration::from_millis(100))
        );
    }

    #[test]
    fn reachability_timeout_globally_and_per_alias() {
        let c = load("", "aliases:\n  a: [{host: a1}]\n").unwrap();
        assert_eq!(c.reachability_timeout.value, Duration::from_millis(500));
        assert_eq!(c.reachability_timeout.origin, None);
        let c = load(
            "reachability_timeout: 2s\naliases:\n  a:\n    reachability_timeout: 250ms\n    hosts: [{host: a1}]\n  b: [{host: b1}]\n",
            "reachability_timeout: 1.5\n",
        )
        .unwrap();
        // The local file's global value replaces the global file's; an
        // alias's own wins over both.
        assert_eq!(c.reachability_timeout.value, Duration::from_millis(1500));
        assert_eq!(c.reachability_timeout.origin.as_ref().unwrap().line, 1);
        let a = c.reachability_timeout_for(c.alias("a").unwrap());
        assert_eq!(a.value, Duration::from_millis(250));
        assert_eq!(a.origin.as_ref().unwrap().line, 4);
        let b = c.reachability_timeout_for(c.alias("b").unwrap());
        assert_eq!(b.value, Duration::from_millis(1500));
        // Set on an alias whose hosts are in the other file.
        let c = load(
            "aliases:\n  a: [{host: a1}]\n",
            "aliases:\n  a:\n    reachability_timeout: 3s\n",
        )
        .unwrap();
        let a = c.alias("a").unwrap();
        assert_eq!(c.reachability_timeout_for(a).value, Duration::from_secs(3));
        assert_eq!(a.entries.len(), 1);
    }

    #[test]
    fn durations_are_read_and_written() {
        for (text, ms) in [
            ("500ms", 500),
            ("0.5s", 500),
            ("0.5", 500),
            ("2s", 2000),
            ("2", 2000),
            ("1ms", 1),
            ("1.25 s", 1250),
            (" 60s ", 60_000),
            ("60000ms", 60_000),
        ] {
            assert_eq!(parse_timeout(text), Ok(Duration::from_millis(ms)), "{text}");
        }
        for (text, why) in [
            ("", "expected a duration"),
            ("ms", "expected a duration"),
            ("s", "expected a duration"),
            (".", "expected a duration"),
            ("-1s", "expected a duration"),
            ("1e3ms", "expected a duration"),
            ("1.2.3", "expected a duration"),
            ("inf", "expected a duration"),
            ("5m", "expected a duration"),
            ("0", "too short"),
            ("0.0001s", "too short"),
            ("61s", "too long: at most 60s"),
            ("99999999999999999999999", "too long"),
        ] {
            let e = parse_timeout(text).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
        assert_eq!(format_timeout(Duration::from_millis(500)), "500ms");
        assert_eq!(format_timeout(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_timeout(Duration::from_secs(2)), "2s");
    }

    #[test]
    fn a_bad_reachability_timeout_names_the_file_and_line() {
        for (local, want) in [
            (
                "reachability_timeout: soon\n",
                ":1: reachability_timeout: expected a duration such as 500ms or 2s, found 'soon'",
            ),
            (
                "reachability_timeout: [1s]\n",
                ":1: reachability_timeout: expected a duration such as 500ms or 2s, found a list",
            ),
            (
                "aliases:\n  d:\n    reachability_timeout: 0ms\n    hosts: [{host: a}]\n",
                ":3: aliases.d: reachability_timeout: '0ms' is too short: at least 1ms",
            ),
        ] {
            let e = load("", local).unwrap_err();
            assert!(e.ends_with(want), "{local:?}: {e}");
        }
    }

    /// acs-9yv: `local_networks` on a host entry, in every form it is
    /// written -- including a single network with no list around it.
    #[test]
    fn local_networks_read_as_a_list_on_a_host_entry() {
        let net = |s: &str| LocalNet::parse(s).unwrap();
        let nets = |c: &Config, a: &str| c.alias(a).unwrap().entries[0].local_networks.clone();
        // A block sequence, a flow one, and one value with commas all read
        // the same.
        for text in [
            "aliases:\n  d:\n    - host: a\n      local_networks:\n        - 172.16.0.0/16\n        - fd00::/48\n",
            "aliases:\n  d:\n    - host: a\n      local_networks: [172.16.0.0/16, fd00::/48]\n",
            "aliases:\n  d:\n    - host: a\n      local_networks: 172.16.0.0/16, fd00::/48\n",
        ] {
            let c = load("", text).unwrap();
            assert_eq!(
                nets(&c, "d"),
                [net("172.16.0.0/16"), net("fd00::/48")],
                "{text:?}"
            );
        }
        // One network needs no list around it: a plain scalar is split on
        // commas, and a lone network has none. IPv6 bare is the case worth
        // pinning -- the address is mostly colons, which YAML gives no
        // meaning unless one is followed by a space.
        for (value, want) in [
            ("172.16.0.0/16", "172.16.0.0/16"),
            ("\"172.16.0.0/16\"", "172.16.0.0/16"),
            ("[172.16.0.0/16]", "172.16.0.0/16"),
            ("fd00::/48", "fd00::/48"),
            ("'fd00::/48'", "fd00::/48"),
            // The address need not be the network's own: the prefix decides.
            ("172.16.8.2/16", "172.16.0.0/16"),
        ] {
            let text = format!("aliases:\n  d:\n    - host: a\n      local_networks: {value}\n");
            let c = load("", &text).unwrap();
            // By the network, not by the address it was written with.
            let got: Vec<String> = nets(&c, "d").iter().map(|n| n.to_string()).collect();
            assert_eq!(got, [want], "{value:?}");
            assert!(nets(&c, "d")[0].contains(net(want).addr), "{value:?}");
        }
        // Each entry has its own; an empty value, and an empty list, set
        // none, as any setting's does.
        let c = load(
            "",
            "aliases:\n  d:\n    - host: a\n      local_networks: [172.16.0.0/16]\n    \
             - host: b\n      local_networks:\n    - host: c\n      local_networks: []\n    \
             - host: e\n",
        )
        .unwrap();
        let of = |i: usize| c.alias("d").unwrap().entries[i].local_networks.clone();
        assert_eq!(of(0), [net("172.16.0.0/16")]);
        for i in 1..4 {
            assert!(of(i).is_empty(), "entry {i}");
        }
        // The setting has no global or per-alias form any more (acs-9yv):
        // it names one entry, so it lives on that entry.
        assert!(!KEYS.contains(&"local_networks"));
        assert!(!ALIAS_KEYS.contains(&"local_networks"));
        assert!(HOST_KEYS.contains(&"local_networks"));
    }

    #[test]
    fn a_bad_global_file_is_an_error_too() {
        let e = load("install_on_remote: nope\n", "").unwrap_err();
        assert!(e.contains("global.yaml:1:"), "{e}");
    }

    #[test]
    fn paths_follow_xdg() {
        // Read the environment only; the process-wide variables are not set
        // here, since tests run in parallel.
        let p = local_path();
        assert!(p.ends_with("acs/config.yaml"), "{}", p.display());
        assert!(global_path().ends_with("config.yaml"));
    }
}
