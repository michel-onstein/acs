//! `acs config …` (DESIGN §7.4): read and edit the configuration files from
//! the command line.
//!
//! Edits go through the YAML tree (`yaml.rs`), so a file keeps its comments
//! and the order of everything the edit did not touch; the result is parsed
//! and validated again before it replaces the file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::config::{self, Config, HostEntry};
use crate::yaml::{self, Document, Node, Value};

pub const USAGE: &str = "\
usage: acs config show                 the merged configuration and where each value is from
       acs config get <key>            one setting's value
       acs config set <key> <value>    change a setting
       acs config unset <key>          remove a setting from the file
       acs config host list            every alias and its hosts
       acs config host add <alias> <host> [--user <login>] [--identity-file <key>]
                           [--no-reachability-check]
                                       add a host to an alias (after its other hosts)
       acs config host remove <alias> [<host>]
                                       remove one host, or the whole alias
       acs config host set <alias> identity_file <key>
                                       the ssh key of the alias's hosts that name none
       acs config host unset <alias> identity_file
                                       remove the alias's key from the file
       acs config path                 the files acs reads

  --global  edit /etc/acs/config.yaml instead of ~/.config/acs/config.yaml

settings: install_on_remote, update_check (true|false)
precedence of the ssh key: -i, then the host's identity_file, then the alias's";

/// Settings `get`/`set`/`unset` know: all true or false.
const SCALARS: &[&str] = config::BOOLS;

/// Settings of an alias that `host set`/`host unset` know.
const ALIAS_SETTINGS: &[&str] = &["identity_file"];

/// What to do, parsed from the arguments after `config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Show,
    Path,
    Get(String),
    Set(String, String),
    Unset(String),
    HostList,
    HostAdd {
        alias: String,
        host: String,
        user: Option<String>,
        check: bool,
        identity_file: Option<String>,
    },
    HostRemove {
        alias: String,
        host: Option<String>,
    },
    HostSet {
        alias: String,
        key: String,
        value: String,
    },
    HostUnset {
        alias: String,
        key: String,
    },
    Help,
}

/// Parse `acs config` arguments; `bool` is `--global`.
pub fn parse(args: &[OsString]) -> Result<(Cmd, bool), String> {
    let mut global = false;
    let mut user = None;
    let mut check = None;
    let mut identity: Option<String> = None;
    let mut words = Vec::new();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--global" => global = true,
            "--user" => user = Some(it.next().ok_or("--user needs a login name")?),
            "--identity-file" => {
                identity = Some(it.next().ok_or("--identity-file needs a key file")?)
            }
            "--no-reachability-check" => check = Some(false),
            "--reachability-check" => check = Some(true),
            "-h" | "--help" => return Ok((Cmd::Help, global)),
            s if s.starts_with("--user=") => user = Some(s["--user=".len()..].to_string()),
            s if s.starts_with("--identity-file=") => {
                identity = Some(s["--identity-file=".len()..].to_string())
            }
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option {s} (see acs config --help)"))
            }
            _ => words.push(a),
        }
    }
    if identity.as_deref() == Some("") {
        return Err("--identity-file needs a key file".into());
    }
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    if let ["host", "set" | "unset", _, key, ..] = w.as_slice() {
        if !ALIAS_SETTINGS.contains(key) {
            return Err(format!(
                "unknown alias setting '{key}' (settings: {})",
                ALIAS_SETTINGS.join(", ")
            ));
        }
    }
    let host_opts = user.is_some() || check.is_some() || identity.is_some();
    let cmd = match w.as_slice() {
        ["show"] => Cmd::Show,
        ["path"] => Cmd::Path,
        ["help"] => Cmd::Help,
        ["get", key] => Cmd::Get(key.to_string()),
        ["set", key, value] => Cmd::Set(key.to_string(), value.to_string()),
        ["unset", key] => Cmd::Unset(key.to_string()),
        ["host", "list"] | ["host", "ls"] => Cmd::HostList,
        ["host", "add", alias, host] => Cmd::HostAdd {
            alias: alias.to_string(),
            host: host.to_string(),
            user,
            check: check.unwrap_or(true),
            identity_file: identity,
        },
        ["host", "remove" | "rm", alias] => Cmd::HostRemove {
            alias: alias.to_string(),
            host: None,
        },
        ["host", "remove" | "rm", alias, host] => Cmd::HostRemove {
            alias: alias.to_string(),
            host: Some(host.to_string()),
        },
        ["host", "set", alias, key, value] if !value.is_empty() => Cmd::HostSet {
            alias: alias.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        },
        ["host", "unset", alias, key] => Cmd::HostUnset {
            alias: alias.to_string(),
            key: key.to_string(),
        },
        [] => return Err("acs config needs a command (see acs config --help)".into()),
        _ => {
            return Err(format!(
                "bad arguments: acs config {} (see acs config --help)",
                w.join(" ")
            ))
        }
    };
    if host_opts && !matches!(cmd, Cmd::HostAdd { .. }) {
        return Err(
            "--user, --identity-file and --[no-]reachability-check go with acs config host add"
                .into(),
        );
    }
    Ok((cmd, global))
}

/// Entry point of `acs config` (arguments after `config`).
pub fn main(args: &[OsString]) -> ExitCode {
    let (cmd, global) = match parse(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(2);
        }
    };
    let target = if global {
        config::global_path()
    } else {
        config::local_path()
    };
    match run(&cmd, &target) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("acs: {e}");
            ExitCode::from(e.code())
        }
    }
}

/// An error and its exit code: 2 for a request that cannot be done as asked,
/// 1 for a file that cannot be written.
#[derive(Debug)]
pub enum Error {
    Usage(String),
    Io(String),
}

impl Error {
    fn code(&self) -> u8 {
        match self {
            Error::Usage(_) => 2,
            Error::Io(_) => 1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage(s) | Error::Io(s) => f.write_str(s),
        }
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::Usage(s)
    }
}

/// Run `cmd`, editing `target` if it edits; returns what to print.
pub fn run(cmd: &Cmd, target: &Path) -> Result<String, Error> {
    let files = [config::global_path(), config::local_path()];
    match cmd {
        Cmd::Help => Ok(format!("{USAGE}\n")),
        Cmd::Path => Ok(paths(&files)),
        Cmd::Show => Ok(show(&Config::load_files(&files)?, &files)),
        Cmd::Get(key) => {
            let c = Config::load_files(&files)?;
            match (key.as_str(), c.bool_setting(key)) {
                (_, Some(s)) => Ok(format!("{}\n", s.value)),
                ("hosts", None) => Ok(host_list(&c)),
                (other, None) => Err(unknown_key(other).into()),
            }
        }
        Cmd::HostList => Ok(host_list(&Config::load_files(&files)?)),
        Cmd::Set(..)
        | Cmd::Unset(..)
        | Cmd::HostAdd { .. }
        | Cmd::HostRemove { .. }
        | Cmd::HostSet { .. }
        | Cmd::HostUnset { .. } => {
            // A setting of an alias with no hosts anywhere would leave a
            // configuration the client refuses.
            if let Cmd::HostSet { alias, .. } = cmd {
                if Config::load_files(&files)?.alias(alias).is_none() {
                    return Err(format!(
                        "no alias '{alias}' (add its first host with: acs config host add {alias} <host>)"
                    )
                    .into());
                }
            }
            let mut doc = config::read_doc(target)?.unwrap_or_else(empty_doc);
            let msg = edit(&mut doc, cmd).map_err(|e| {
                // Point at the other file when the thing is defined there.
                let other = if target == files[0] {
                    &files[1]
                } else {
                    &files[0]
                };
                match defined_in(other, cmd) {
                    Some(hint) => format!("{e}; {hint}"),
                    None => e,
                }
            })?;
            together(&doc, target, &files)?;
            write(target, &doc)?;
            Ok(format!("{msg} in {}\n", pretty(target)))
        }
    }
}

/// Refuse an edit that leaves `doc`, merged with the other file, a
/// configuration the client refuses: an alias's identity_file in one file
/// whose hosts were all in the other. A broken file is left to `write`
/// (this one) or alone (the other).
fn together(doc: &Document, target: &Path, files: &[PathBuf; 2]) -> Result<(), String> {
    let mut c = Config::default();
    for f in files {
        if f == target {
            let _ = c.apply(f, &doc.root);
        } else if let Ok(Some(other)) = config::read_doc(f) {
            let _ = c.apply(f, &other.root);
        }
    }
    c.check()
        .map_err(|e| format!("{e} once this edit is made (not saved)"))
}

fn empty_doc() -> Document {
    Document {
        root: Node::new(Value::Null),
        tail: Vec::new(),
    }
}

fn unknown_key(k: &str) -> String {
    let hint = if config::HOST_KEYS.contains(&k) || k.starts_with("hosts.") {
        " (hosts are edited with acs config host add|remove|set|unset)"
    } else {
        ""
    };
    format!(
        "unknown setting '{k}' (settings: {}){hint}",
        SCALARS.join(", ")
    )
}

/// A path with the home directory shown as `~`.
pub fn pretty(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn paths(files: &[PathBuf; 2]) -> String {
    let state = |p: &Path| if p.exists() { "" } else { " (not found)" };
    format!(
        "global: {}{}\nlocal:  {}{}\n",
        files[0].display(),
        state(&files[0]),
        pretty(&files[1]),
        state(&files[1])
    )
}

// ---- reading ---------------------------------------------------------------

fn from(origin: &Option<config::Origin>) -> String {
    match origin {
        Some(o) => format!("# {}:{}", pretty(&o.file), o.line),
        None => "# default".into(),
    }
}

/// The merged configuration as YAML, each value commented with its origin.
pub fn show(c: &Config, files: &[PathBuf; 2]) -> String {
    let mut root = Vec::new();
    for key in SCALARS {
        let s = c.bool_setting(key).expect("a bool setting");
        root.push((
            key.to_string(),
            Node {
                comment: Some(from(&s.origin)),
                ..Node::bool(s.value)
            },
        ));
    }
    let mut aliases = Vec::new();
    for a in &c.hosts {
        let items = a
            .entries
            .iter()
            .map(|e| {
                let mut n = entry_node(e);
                if let Some(m) = n.value.map_mut() {
                    m[0].1.comment = Some(from(&Some(e.origin.clone())));
                }
                n
            })
            .collect();
        let list = Node::new(Value::Seq(items));
        // An alias with a setting of its own is written as a mapping.
        let node = match &a.identity_file {
            None => list,
            Some(key) => Node::new(Value::Map(vec![
                (
                    "identity_file".to_string(),
                    Node {
                        comment: Some(from(&key.origin)),
                        ..Node::string(&key.value)
                    },
                ),
                ("hosts".to_string(), list),
            ])),
        };
        aliases.push((a.name.clone(), node));
    }
    if aliases.is_empty() {
        root.push((
            "hosts".to_string(),
            Node {
                comment: Some("# none".into()),
                flow: true,
                ..Node::new(Value::Map(Vec::new()))
            },
        ));
    } else {
        root.push(("hosts".to_string(), Node::new(Value::Map(aliases))));
    }
    let mut head = vec!["# acs configuration, merged from:".to_string()];
    for f in files {
        let state = if c.files.contains(f) {
            ""
        } else {
            " (not found)"
        };
        head.push(format!("#   {}{state}", pretty(f)));
    }
    yaml::emit(&Document {
        root: Node {
            before: head,
            ..Node::new(Value::Map(root))
        },
        tail: Vec::new(),
    })
}

/// One host entry as YAML: `host`, then `user`, `identity_file` and
/// `reachability_check` when they are not the defaults.
fn entry_node(e: &HostEntry) -> Node {
    let mut m = vec![("host".to_string(), Node::string(&e.host))];
    if let Some(u) = &e.user {
        m.push(("user".to_string(), Node::string(u)));
    }
    if let Some(key) = &e.identity_file {
        m.push(("identity_file".to_string(), Node::string(&key.value)));
    }
    if !e.reachability_check {
        m.push(("reachability_check".to_string(), Node::bool(false)));
    }
    Node::new(Value::Map(m))
}

/// Every alias and its hosts, in the order they are tried, each with the
/// ssh key it is reached with (its own, or the alias's).
pub fn host_list(c: &Config) -> String {
    if c.hosts.is_empty() {
        return "no host aliases (add one with: acs config host add <alias> <host>)\n".into();
    }
    let mut rows = vec![[
        "ALIAS".to_string(),
        "HOST".into(),
        "USER".into(),
        "CHECK".into(),
        "IDENTITY".into(),
        "FROM".into(),
    ]];
    for a in &c.hosts {
        for e in &a.entries {
            rows.push([
                a.name.clone(),
                e.host.clone(),
                e.user.clone().unwrap_or_else(|| "-".into()),
                if e.reachability_check { "ping" } else { "none" }.into(),
                e.identity_file
                    .as_ref()
                    .or(a.identity_file.as_ref())
                    .map_or_else(|| "-".into(), |k| k.value.clone()),
                format!("{}:{}", pretty(&e.origin.file), e.origin.line),
            ]);
        }
    }
    let mut widths = [0usize; 6];
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in &rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i < 5 {
                line.push_str(&format!("{c:<w$}  ", w = widths[i]));
            } else {
                line.push_str(c);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Where `cmd`'s key or alias is defined in `other`, as a hint for an edit
/// that found nothing to change in its own file.
fn defined_in(other: &Path, cmd: &Cmd) -> Option<String> {
    let doc = config::read_doc(other).ok()??;
    let map = doc.root.value.map()?;
    let find = |k: &str| map.iter().find(|(key, _)| key == k).map(|(_, n)| n);
    let alias = |name: &str| {
        find("hosts")?
            .value
            .map()?
            .iter()
            .find(|(a, _)| a == name)
            .map(|(_, n)| n)
    };
    let node = match cmd {
        Cmd::Unset(key) => find(key)?,
        Cmd::HostRemove { alias: a, .. } => alias(a)?,
        Cmd::HostUnset { alias: a, key } => Some(alias(a)?)
            .filter(|n| is_alias_form(n))?
            .value
            .map()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, n)| n)?,
        _ => return None,
    };
    let flag = if other == config::global_path() {
        "--global"
    } else {
        "no --global"
    };
    Some(format!(
        "it is set in {}:{} (use {flag})",
        pretty(other),
        node.line
    ))
}

// ---- editing ---------------------------------------------------------------

fn root_map(doc: &mut Document) -> Result<&mut Vec<(String, Node)>, String> {
    if doc.root.value == Value::Null {
        doc.root.value = Value::Map(Vec::new());
        doc.root.flow = false;
        // A file of comments only: they head the settings added below them.
        let tail = std::mem::take(&mut doc.tail);
        doc.root.before.extend(tail);
    }
    doc.root
        .value
        .map_mut()
        .ok_or_else(|| "the file does not hold 'key: value' settings; fix it by hand".into())
}

fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} is true or false, not '{v}'")),
    }
}

/// Apply an edit to `doc`; returns what was done.
pub fn edit(doc: &mut Document, cmd: &Cmd) -> Result<String, String> {
    match cmd {
        Cmd::Set(key, value) => {
            let node = match key.as_str() {
                k if SCALARS.contains(&k) => Node::bool(parse_bool(key, value)?),
                other => return Err(unknown_key(other)),
            };
            let map = root_map(doc)?;
            match map.iter_mut().find(|(k, _)| k == key) {
                Some((_, n)) => {
                    n.value = node.value;
                    n.flow = false;
                }
                None => map.push((key.clone(), node)),
            }
            Ok(format!("set {key} to {value}"))
        }
        Cmd::Unset(key) => {
            if !SCALARS.contains(&key.as_str()) {
                return Err(unknown_key(key));
            }
            let map = root_map(doc)?;
            let Some(i) = map.iter().position(|(k, _)| k == key) else {
                return Err(format!("{key} is not set in this file"));
            };
            remove_entry(map, i);
            Ok(format!("removed {key}"))
        }
        Cmd::HostAdd {
            alias,
            host,
            user,
            check,
            identity_file,
        } => {
            config::validate_alias(alias)?;
            let entry = HostEntry {
                host: host.clone(),
                user: user.clone(),
                reachability_check: *check,
                identity_file: identity_file.clone().map(|value| config::Setting {
                    value,
                    origin: None,
                }),
                origin: config::Origin {
                    file: PathBuf::new(),
                    line: 0,
                },
            };
            let items = entry_list(alias, alias_node(aliases_mut(doc)?, alias))?;
            if items.iter().any(|n| entry_host(n) == Some(host.as_str())) {
                return Err(format!("{host} is already a host of {alias}"));
            }
            items.push(entry_node(&entry));
            let place = match items.len() {
                1 => "its only host".to_string(),
                n => format!("host {n}"),
            };
            let key = match identity_file {
                Some(k) => format!(", with identity_file {k}"),
                None => String::new(),
            };
            Ok(format!(
                "added {} to {alias} as {place}{key}",
                entry.destination()
            ))
        }
        Cmd::HostSet { alias, key, value } => {
            config::validate_alias(alias)?;
            let settings = alias_form(alias_node(aliases_mut(doc)?, alias));
            let new = Node::string(value);
            match settings.iter_mut().find(|(k, _)| k == key) {
                Some((_, n)) => {
                    n.value = new.value;
                    n.flow = false;
                }
                // Settings go before the hosts.
                None => settings.insert(0, (key.clone(), new)),
            }
            Ok(format!("set {key} of {alias} to {value}"))
        }
        Cmd::HostUnset { alias, key } => {
            let missing = || format!("{key} of {alias} is not set in this file");
            let map = root_map(doc)?;
            let hi = map
                .iter()
                .position(|(k, _)| k == "hosts")
                .ok_or_else(missing)?;
            let aliases = map[hi].1.value.map_mut().ok_or_else(missing)?;
            let ai = aliases
                .iter()
                .position(|(a, _)| a == alias)
                .ok_or_else(missing)?;
            let node = &mut aliases[ai].1;
            if !is_alias_form(node) {
                return Err(missing());
            }
            let settings = node.value.map_mut().expect("a mapping");
            let ki = settings
                .iter()
                .position(|(k, _)| k == key)
                .ok_or_else(missing)?;
            remove_entry(settings, ki);
            let left: Vec<String> = settings
                .iter()
                .filter(|(_, n)| n.value != Value::Null)
                .map(|(k, _)| k.clone())
                .collect();
            if left.is_empty() {
                // Only the setting was here: the hosts are in the other file.
                remove_entry(aliases, ai);
                if aliases.is_empty() {
                    remove_entry(map, hi);
                }
                return Ok(format!(
                    "removed {key} of {alias}, and {alias}, which has no hosts in this file"
                ));
            }
            if left == ["hosts"] {
                // Back to the plain list of hosts.
                let i = settings.iter().position(|(k, _)| k == "hosts").unwrap();
                let (_, hosts) = settings.remove(i);
                node.value = hosts.value;
                node.flow = hosts.flow;
            }
            Ok(format!("removed {key} of {alias}"))
        }
        Cmd::HostRemove { alias, host } => {
            let map = root_map(doc)?;
            let Some(hi) = map.iter().position(|(k, _)| k == "hosts") else {
                return Err(format!("no alias '{alias}' in this file"));
            };
            let aliases = map[hi]
                .1
                .value
                .map_mut()
                .ok_or(format!("no alias '{alias}' in this file"))?;
            let Some(ai) = aliases.iter().position(|(a, _)| a == alias) else {
                return Err(format!("no alias '{alias}' in this file"));
            };
            let msg = match host {
                None => {
                    remove_entry(aliases, ai);
                    format!("removed alias {alias}")
                }
                Some(h) => {
                    let (removed, empty) = match listed_hosts(&mut aliases[ai].1) {
                        None => (0, true),
                        Some(list) => {
                            let removed = match &mut list.value {
                                Value::Seq(items) => {
                                    let before = items.len();
                                    let mut i = 0;
                                    while i < items.len() {
                                        if entry_host(&items[i]) == Some(h.as_str()) {
                                            let gone = items.remove(i);
                                            keep_comments(gone.before, items.get_mut(i));
                                        } else {
                                            i += 1;
                                        }
                                    }
                                    before - items.len()
                                }
                                Value::Map(_) => usize::from(entry_host(list) == Some(h.as_str())),
                                _ => 0,
                            };
                            let empty = match &list.value {
                                Value::Seq(v) => v.is_empty(),
                                _ => true,
                            };
                            (removed, empty)
                        }
                    };
                    if removed == 0 {
                        return Err(format!("{h} is not a host of {alias} in this file"));
                    }
                    if empty {
                        remove_entry(aliases, ai);
                        format!("removed {h}, the last host of {alias}, and the alias")
                    } else {
                        format!("removed {h} from {alias}")
                    }
                }
            };
            if aliases.is_empty() {
                remove_entry(map, hi);
            }
            Ok(msg)
        }
        _ => unreachable!("not an edit"),
    }
}

/// The `hosts` mapping of `doc`, created if missing.
fn aliases_mut(doc: &mut Document) -> Result<&mut Vec<(String, Node)>, String> {
    let map = root_map(doc)?;
    let i = match map.iter().position(|(k, _)| k == "hosts") {
        Some(i) => i,
        None => {
            map.push(("hosts".into(), Node::new(Value::Null)));
            map.len() - 1
        }
    };
    let hosts = &mut map[i].1;
    if hosts.value == Value::Null {
        hosts.value = Value::Map(Vec::new());
    }
    hosts.flow = false;
    hosts
        .value
        .map_mut()
        .ok_or_else(|| "'hosts' is not a mapping of aliases; fix it by hand".into())
}

/// The node of `alias` in `aliases`, added empty if missing.
fn alias_node<'a>(aliases: &'a mut Vec<(String, Node)>, alias: &str) -> &'a mut Node {
    let i = match aliases.iter().position(|(a, _)| a == alias) {
        Some(i) => i,
        None => {
            aliases.push((alias.to_string(), Node::new(Value::Null)));
            aliases.len() - 1
        }
    };
    &mut aliases[i].1
}

/// Whether an alias's node is the mapping of its settings and `hosts`
/// rather than its list of hosts or its one host entry (DESIGN §7.2).
fn is_alias_form(n: &Node) -> bool {
    matches!(&n.value, Value::Map(m) if !m.iter().any(|(k, _)| k == "host"))
}

/// The node listing an alias's hosts: the alias's own, or its `hosts`.
fn listed_hosts(node: &mut Node) -> Option<&mut Node> {
    if !is_alias_form(node) {
        return Some(node);
    }
    node.value
        .map_mut()?
        .iter_mut()
        .find(|(k, _)| k == "hosts")
        .map(|(_, n)| n)
}

/// An alias's hosts as a block list to add to: one entry written without a
/// list becomes a list, and an alias written as a mapping gets `hosts` if
/// it has none.
fn entry_list<'a>(alias: &str, node: &'a mut Node) -> Result<&'a mut Vec<Node>, String> {
    let list = if is_alias_form(node) {
        node.flow = false;
        let settings = node.value.map_mut().expect("a mapping");
        let i = match settings.iter().position(|(k, _)| k == "hosts") {
            Some(i) => i,
            None => {
                settings.push(("hosts".into(), Node::new(Value::Null)));
                settings.len() - 1
            }
        };
        &mut settings[i].1
    } else {
        node
    };
    match &list.value {
        Value::Null => list.value = Value::Seq(Vec::new()),
        Value::Map(_) => {
            let single = Node {
                value: std::mem::replace(&mut list.value, Value::Null),
                flow: list.flow,
                ..Node::new(Value::Null)
            };
            list.value = Value::Seq(vec![single]);
        }
        _ => {}
    }
    list.flow = false;
    match &mut list.value {
        Value::Seq(items) => Ok(items),
        _ => Err(format!(
            "hosts.{alias} is not a list of hosts; fix it by hand"
        )),
    }
}

/// An alias's node as the mapping of its settings, its list of hosts (or
/// one entry, made a list) moved under `hosts`, so a setting can go beside
/// them.
fn alias_form(node: &mut Node) -> &mut Vec<(String, Node)> {
    if !is_alias_form(node) {
        let hosts = match std::mem::replace(&mut node.value, Value::Null) {
            Value::Null => None,
            single @ Value::Map(_) => Some(Node::new(Value::Seq(vec![Node {
                flow: node.flow,
                ..Node::new(single)
            }]))),
            list => Some(Node {
                flow: node.flow,
                ..Node::new(list)
            }),
        };
        node.value = Value::Map(
            hosts
                .map(|h| ("hosts".to_string(), h))
                .into_iter()
                .collect(),
        );
    }
    node.flow = false;
    node.value.map_mut().expect("a mapping")
}

/// The `host:` of a host entry node.
fn entry_host(n: &Node) -> Option<&str> {
    let m = n.value.map()?;
    match &m.iter().find(|(k, _)| k == "host")?.1.value {
        Value::Scalar(s) => Some(&s.text),
        _ => None,
    }
}

/// Remove entry `i`, keeping the comment lines above it for what follows.
fn remove_entry(map: &mut Vec<(String, Node)>, i: usize) {
    let (_, gone) = map.remove(i);
    keep_comments(gone.before, map.get_mut(i).map(|(_, n)| n));
}

/// Put the comment lines of a removed node above the node that took its
/// place (they are dropped with the last one).
fn keep_comments(mut before: Vec<String>, next: Option<&mut Node>) {
    if let Some(next) = next {
        before.append(&mut next.before);
        next.before = before;
    }
}

/// Validate `doc` as a configuration file and write it in place atomically.
fn write(path: &Path, doc: &Document) -> Result<(), Error> {
    let text = yaml::emit(doc);
    let check = yaml::parse(&text).map_err(|e| Error::Io(format!("internal error: {e}")))?;
    Config::default()
        .apply(path, &check.root)
        .map_err(|e| Error::Usage(format!("{e} (not saved; fix the file by hand)")))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let io = |e: std::io::Error, what: &Path| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (run with sudo for --global)"
        } else {
            ""
        };
        Error::Io(format!("cannot write {}: {e}{hint}", what.display()))
    };
    std::fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
    let tmp = dir.join(format!(
        ".config.yaml.{:08x}",
        crate::sys::random_u64() as u32
    ));
    std::fs::write(&tmp, &text).map_err(|e| io(e, path))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e, path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    fn cmd(s: &str) -> Cmd {
        parse(&args(s)).unwrap().0
    }

    fn apply(src: &str, c: &str) -> Result<String, String> {
        let mut doc = yaml::parse(src).unwrap();
        edit(&mut doc, &cmd(c))?;
        Ok(yaml::emit(&doc))
    }

    #[test]
    fn parses_every_subcommand() {
        assert_eq!(cmd("show"), Cmd::Show);
        assert_eq!(cmd("path"), Cmd::Path);
        assert_eq!(
            cmd("get install_on_remote"),
            Cmd::Get("install_on_remote".into())
        );
        assert_eq!(
            cmd("set install_on_remote false"),
            Cmd::Set("install_on_remote".into(), "false".into())
        );
        assert_eq!(cmd("unset x"), Cmd::Unset("x".into()));
        assert_eq!(cmd("host list"), Cmd::HostList);
        assert_eq!(
            cmd("host add devbox devbox.lan --user me --no-reachability-check"),
            Cmd::HostAdd {
                alias: "devbox".into(),
                host: "devbox.lan".into(),
                user: Some("me".into()),
                check: false,
                identity_file: None
            }
        );
        assert_eq!(
            cmd("host add devbox devbox.lan --reachability-check --user=me --identity-file=~/k"),
            Cmd::HostAdd {
                alias: "devbox".into(),
                host: "devbox.lan".into(),
                user: Some("me".into()),
                check: true,
                identity_file: Some("~/k".into())
            }
        );
        assert_eq!(
            cmd("host add devbox devbox.lan --identity-file /keys/k"),
            Cmd::HostAdd {
                alias: "devbox".into(),
                host: "devbox.lan".into(),
                user: None,
                check: true,
                identity_file: Some("/keys/k".into())
            }
        );
        assert_eq!(
            cmd("host set devbox identity_file ~/.ssh/id_devbox"),
            Cmd::HostSet {
                alias: "devbox".into(),
                key: "identity_file".into(),
                value: "~/.ssh/id_devbox".into()
            }
        );
        assert_eq!(
            cmd("host unset devbox identity_file"),
            Cmd::HostUnset {
                alias: "devbox".into(),
                key: "identity_file".into()
            }
        );
        assert_eq!(
            cmd("host rm devbox"),
            Cmd::HostRemove {
                alias: "devbox".into(),
                host: None
            }
        );
        assert_eq!(
            parse(&args("--global host list")).unwrap(),
            (Cmd::HostList, true)
        );
        assert_eq!(cmd("--help"), Cmd::Help);
    }

    #[test]
    fn bad_arguments_are_explained() {
        for (a, want) in [
            ("", "needs a command"),
            ("frobnicate", "bad arguments"),
            ("get", "bad arguments"),
            ("set install_on_remote", "bad arguments"),
            ("show --user me", "go with acs config host add"),
            ("host add a", "bad arguments"),
            ("show --bogus", "unknown option --bogus"),
            ("host add a b --user", "--user needs"),
            ("host add a b --identity-file", "--identity-file needs"),
            ("host add a b --identity-file=", "--identity-file needs"),
            (
                "host remove a --identity-file k",
                "go with acs config host add",
            ),
            ("host set a port 22", "unknown alias setting 'port'"),
            ("host unset a user", "unknown alias setting 'user'"),
            ("host set a identity_file", "bad arguments"),
        ] {
            let e = parse(&args(a)).unwrap_err();
            assert!(e.contains(want), "{a}: {e}");
        }
    }

    #[test]
    fn set_replaces_in_place_and_keeps_comments() {
        let src = "# mine\ninstall_on_remote: true # was on\nhosts:\n  a:\n    - host: x\n";
        assert_eq!(
            apply(src, "set install_on_remote false").unwrap(),
            "# mine\ninstall_on_remote: false # was on\nhosts:\n  a:\n    - host: x\n"
        );
        // A new setting goes at the end.
        assert_eq!(
            apply("# empty\n", "set install_on_remote FALSE").unwrap(),
            "# empty\ninstall_on_remote: false\n"
        );
        let e = apply("", "set install_on_remote maybe").unwrap_err();
        assert!(e.contains("true or false, not 'maybe'"), "{e}");
        let e = apply("", "set port 22").unwrap_err();
        assert!(e.contains("unknown setting 'port'"), "{e}");
        let e = apply("", "set host x").unwrap_err();
        assert!(e.contains("acs config host add"), "{e}");
    }

    #[test]
    fn unset_removes_and_leaves_the_comments_above_for_what_follows() {
        let src = "install_on_remote: false\n# the hosts\nhosts:\n  a:\n    - host: x\n";
        let out = apply(src, "unset install_on_remote").unwrap();
        assert_eq!(out, "# the hosts\nhosts:\n  a:\n    - host: x\n");
        let e = apply(&out, "unset install_on_remote").unwrap_err();
        assert!(e.contains("not set in this file"), "{e}");
    }

    #[test]
    fn host_add_creates_appends_and_refuses_duplicates() {
        let out = apply("install_on_remote: true\n", "host add devbox devbox.lan").unwrap();
        assert_eq!(
            out,
            "install_on_remote: true\nhosts:\n  devbox:\n    - host: devbox.lan\n"
        );
        let out = apply(
            &out,
            "host add devbox devbox.example.com --user me --no-reachability-check",
        )
        .unwrap();
        assert_eq!(
            out,
            "install_on_remote: true\nhosts:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n      user: me\n      reachability_check: false\n"
        );
        let e = apply(&out, "host add devbox devbox.lan").unwrap_err();
        assert!(e.contains("already a host of devbox"), "{e}");
        let e = apply("", "host add a@b x").unwrap_err();
        assert!(e.contains("bad alias name"), "{e}");
    }

    #[test]
    fn host_add_turns_a_single_entry_into_a_list() {
        let out = apply(
            "hosts:\n  nas: {host: nas.lan}\n",
            "host add nas nas.example.com",
        )
        .unwrap();
        assert_eq!(
            out,
            "hosts:\n  nas:\n    - {host: nas.lan}\n    - host: nas.example.com\n"
        );
    }

    #[test]
    fn host_remove_one_host_or_the_alias() {
        let src = "hosts:\n  d:\n    - host: a\n    - host: b\n  e:\n    - host: c\n";
        assert_eq!(
            apply(src, "host remove d a").unwrap(),
            "hosts:\n  d:\n    - host: b\n  e:\n    - host: c\n"
        );
        assert_eq!(
            apply(src, "host remove d").unwrap(),
            "hosts:\n  e:\n    - host: c\n"
        );
        // The last host takes the alias with it, and the last alias `hosts`.
        assert_eq!(
            apply("hosts:\n  e:\n    - host: c\n", "host remove e c").unwrap(),
            ""
        );
        let e = apply(src, "host remove d zz").unwrap_err();
        assert!(e.contains("zz is not a host of d"), "{e}");
        let e = apply(src, "host remove nope").unwrap_err();
        assert!(e.contains("no alias 'nope'"), "{e}");
    }

    #[test]
    fn host_add_with_a_key() {
        assert_eq!(
            apply("", "host add d a --user me --identity-file ~/.ssh/id_a").unwrap(),
            "hosts:\n  d:\n    - host: a\n      user: me\n      identity_file: ~/.ssh/id_a\n"
        );
        let mut doc = yaml::parse("").unwrap();
        let msg = edit(&mut doc, &cmd("host add d a --identity-file /k")).unwrap();
        assert_eq!(msg, "added a to d as its only host, with identity_file /k");
    }

    #[test]
    fn host_set_moves_the_hosts_under_the_alias_and_unset_moves_them_back() {
        let src = "hosts:\n  # the dev box\n  d: # mine\n    - host: a\n    - host: b\n";
        let set = apply(src, "host set d identity_file ~/.ssh/id_d").unwrap();
        assert_eq!(
            set,
            "hosts:\n  # the dev box\n  d: # mine\n    identity_file: ~/.ssh/id_d\n    hosts:\n      - host: a\n      - host: b\n"
        );
        // Set again: replaced in place.
        assert_eq!(
            apply(&set, "host set d identity_file /k").unwrap(),
            set.replace("~/.ssh/id_d", "/k")
        );
        // Adding and removing hosts works on the alias's `hosts`.
        let added = apply(&set, "host add d c").unwrap();
        assert!(
            added.ends_with("      - host: b\n      - host: c\n"),
            "{added}"
        );
        assert_eq!(
            apply(&set, "host remove d a").unwrap(),
            "hosts:\n  # the dev box\n  d: # mine\n    identity_file: ~/.ssh/id_d\n    hosts:\n      - host: b\n"
        );
        // The last host takes the alias, key and all.
        assert_eq!(
            apply(&apply(&set, "host remove d a").unwrap(), "host remove d b").unwrap(),
            ""
        );
        assert_eq!(apply(&set, "host unset d identity_file").unwrap(), src);
        let e = apply(src, "host unset d identity_file").unwrap_err();
        assert!(
            e.contains("identity_file of d is not set in this file"),
            "{e}"
        );
    }

    #[test]
    fn host_set_on_one_entry_or_an_alias_of_the_other_file() {
        // One entry without a list becomes the alias's first host.
        assert_eq!(
            apply(
                "hosts:\n  nas: {host: nas.lan}\n",
                "host set nas identity_file k"
            )
            .unwrap(),
            "hosts:\n  nas:\n    identity_file: k\n    hosts:\n      - {host: nas.lan}\n"
        );
        // An alias whose hosts are in the other file: the key alone.
        let only = apply("update_check: false\n", "host set d identity_file k").unwrap();
        assert_eq!(
            only,
            "update_check: false\nhosts:\n  d:\n    identity_file: k\n"
        );
        let doc = yaml::parse(&only).unwrap();
        Config::default().apply(Path::new("x"), &doc.root).unwrap();
        // Unsetting it takes the alias, and an empty `hosts`, too.
        let mut doc = yaml::parse(&only).unwrap();
        let msg = edit(&mut doc, &cmd("host unset d identity_file")).unwrap();
        assert_eq!(yaml::emit(&doc), "update_check: false\n");
        assert!(msg.contains("which has no hosts in this file"), "{msg}");
        // Adding a host to it gives it `hosts`.
        assert_eq!(
            apply(&only, "host add d a").unwrap(),
            "update_check: false\nhosts:\n  d:\n    identity_file: k\n    hosts:\n      - host: a\n"
        );
    }

    #[test]
    fn host_list_is_a_table_in_trial_order() {
        let dir = crate::testutil::TempDir::new();
        let f = dir.path().join("c.yaml");
        std::fs::write(
            &f,
            "hosts:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n      user: me\n      reachability_check: false\n  lab:\n    identity_file: /k/lab\n    hosts:\n      - host: lab1\n        identity_file: /k/lab1\n      - host: lab2\n",
        )
        .unwrap();
        let c = Config::load_files(std::slice::from_ref(&f)).unwrap();
        let out = host_list(&c);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "ALIAS   HOST                USER  CHECK  IDENTITY  FROM"
        );
        assert!(
            lines[1].starts_with("devbox  devbox.lan          -     ping   -         "),
            "{out}"
        );
        assert!(lines[1].ends_with("c.yaml:3"), "{out}");
        assert!(
            lines[2].starts_with("devbox  devbox.example.com  me    none   -         "),
            "{out}"
        );
        // Each host's key: its own, or the alias's.
        assert!(
            lines[3].starts_with("lab     lab1                -     ping   /k/lab1   "),
            "{out}"
        );
        assert!(
            lines[4].starts_with("lab     lab2                -     ping   /k/lab    "),
            "{out}"
        );
        assert_eq!(
            host_list(&Config::default()),
            "no host aliases (add one with: acs config host add <alias> <host>)\n"
        );
    }

    #[test]
    fn show_comments_every_value_with_its_origin() {
        let dir = crate::testutil::TempDir::new();
        let g = dir.path().join("g.yaml");
        let l = dir.path().join("l.yaml");
        std::fs::write(&l, "hosts:\n  d:\n    - host: a\n      user: me\n").unwrap();
        let files = [g, l.clone()];
        let c = Config::load_files(&files).unwrap();
        let out = show(&c, &files);
        assert!(out.contains("g.yaml (not found)\n"), "{out}");
        assert!(out.contains("install_on_remote: true # default\n"), "{out}");
        assert!(
            out.contains(&format!(
                "    - host: a # {}:3\n      user: me\n",
                l.display()
            )),
            "{out}"
        );
        // What show prints is itself a valid configuration.
        let doc = yaml::parse(&out).unwrap();
        Config::default().apply(Path::new("x"), &doc.root).unwrap();
    }

    #[test]
    fn show_writes_an_alias_with_a_key_as_a_mapping() {
        let dir = crate::testutil::TempDir::new();
        let g = dir.path().join("g.yaml");
        let l = dir.path().join("l.yaml");
        std::fs::write(&g, "hosts:\n  d:\n    identity_file: /etc/k\n    hosts:\n      - host: a\n        identity_file: ~/.ssh/a\n").unwrap();
        std::fs::write(&l, "hosts:\n  d:\n    identity_file: ~/.ssh/d\n").unwrap();
        let files = [g.clone(), l.clone()];
        let c = Config::load_files(&files).unwrap();
        let out = show(&c, &files);
        assert!(
            out.contains(&format!(
                "  d:\n    identity_file: ~/.ssh/d # {}:3\n    hosts:\n      - host: a # {}:5\n        identity_file: ~/.ssh/a\n",
                l.display(),
                g.display()
            )),
            "{out}"
        );
        let doc = yaml::parse(&out).unwrap();
        let mut again = Config::default();
        again.apply(Path::new("x"), &doc.root).unwrap();
        assert_eq!(
            again.hosts[0].identity_file.as_ref().unwrap().value,
            "~/.ssh/d"
        );
    }
}
