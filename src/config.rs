//! The client's configuration file (DESIGN §7.2): YAML at
//! `/etc/acs/config.yaml` (global), then `$XDG_CONFIG_HOME/acs/config.yaml`
//! (local, default `~/.config/acs/config.yaml`). Either may be missing.
//!
//! Merging: a setting in the local file replaces the global one; mappings
//! (`hosts`) merge key by key; lists (an alias's hosts) concatenate, global
//! entries first. An empty value (`key:`) sets nothing.
//!
//! Only the local client reads it; the remote roles never do.

use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Install acs on a remote that lacks it (default true).
    pub install_on_remote: Setting<bool>,
    /// Aliases in the order first defined, each with its hosts in order.
    pub hosts: Vec<(String, Vec<HostEntry>)>,
    /// The files that were read, global first.
    pub files: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            install_on_remote: Setting::default(true),
            hosts: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// Every top-level setting, for error messages and `acs config`.
pub const KEYS: &[&str] = &["install_on_remote", "hosts"];

/// Every key of a host entry.
pub const HOST_KEYS: &[&str] = &["host", "user", "reachability_check"];

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
        Ok(c)
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
                "install_on_remote" => {
                    self.install_on_remote = Setting {
                        value: bool_value(node).map_err(|e| fail(node, format!("{key}: {e}")))?,
                        origin: Some(at(node)),
                    }
                }
                "hosts" => {
                    let aliases = node.value.map().ok_or_else(|| {
                        fail(
                            node,
                            format!(
                                "hosts: expected a mapping of alias names, found {}",
                                node.value.kind()
                            ),
                        )
                    })?;
                    for (alias, list) in aliases {
                        validate_alias(alias).map_err(|e| fail(list, e))?;
                        let items: Vec<&Node> = match &list.value {
                            Value::Seq(v) => v.iter().collect(),
                            Value::Map(_) => vec![list],
                            _ => {
                                return Err(fail(
                                    list,
                                    format!(
                                        "hosts.{alias}: expected a list of hosts ('- host: name'), found {}",
                                        list.value.kind()
                                    ),
                                ))
                            }
                        };
                        let mut new = Vec::new();
                        for item in items {
                            new.push(
                                host_entry(item, &at)
                                    .map_err(|e| fail(item, format!("hosts.{alias}: {e}")))?,
                            );
                        }
                        if new.is_empty() {
                            return Err(fail(list, format!("hosts.{alias}: no hosts listed")));
                        }
                        match self.hosts.iter_mut().find(|(a, _)| a == alias) {
                            Some((_, have)) => have.extend(new),
                            None => self.hosts.push((alias.clone(), new)),
                        }
                    }
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

    /// The hosts of `alias`, if it is one.
    pub fn alias(&self, name: &str) -> Option<&[HostEntry]> {
        self.hosts
            .iter()
            .find(|(a, _)| a == name)
            .map(|(_, v)| v.as_slice())
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

fn string_value(n: &Node) -> Result<String, String> {
    match &n.value {
        Value::Scalar(s) if !s.text.is_empty() => Ok(s.text.clone()),
        Value::Scalar(_) => Err("expected a name, found an empty string".into()),
        other => Err(format!("expected a name, found {}", other.kind())),
    }
}

/// Name the field (and its line) in an error about its value.
fn field<T>(key: &str, n: &Node, r: Result<T, String>) -> Result<T, String> {
    r.map_err(|e| format!("{key}: {e} (line {})", n.line))
}

fn host_entry(item: &Node, at: &dyn Fn(&Node) -> Origin) -> Result<HostEntry, String> {
    let fields = item.value.map().ok_or_else(|| {
        format!(
            "expected a host entry ('host: name', optional 'user' and 'reachability_check'), found {}",
            item.value.kind()
        )
    })?;
    let mut host = None;
    let mut user = None;
    let mut check = true;
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
        let g = "hosts:\n  devbox:\n    - host: devbox.lan\n  nas:\n    - host: nas.lan\n      reachability_check: false\n";
        let l = "hosts:\n  devbox:\n    - host: devbox.example.com\n      user: me\n  pi:\n    host: pi.lan\n";
        let c = load(g, l).unwrap();
        assert_eq!(
            hosts(&c, "devbox"),
            ["devbox.lan/true", "me@devbox.example.com/true"]
        );
        assert_eq!(hosts(&c, "nas"), ["nas.lan/false"]);
        // A single entry needs no list.
        assert_eq!(hosts(&c, "pi"), ["pi.lan/true"]);
        let order: Vec<&str> = c.hosts.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(order, ["devbox", "nas", "pi"]);
        let second = &c.alias("devbox").unwrap()[1];
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
                ":1: unknown setting 'x' (known: install_on_remote, hosts)",
            ),
            ("hosts: [a]\n", ":1: hosts: expected a mapping"),
            (
                "hosts:\n  d: foo\n",
                ":2: hosts.d: expected a list of hosts",
            ),
            (
                "hosts:\n  d:\n    - user: me\n",
                ":3: hosts.d: a host entry needs 'host: <name>'",
            ),
            (
                "hosts:\n  d:\n    - host: a\n      port: 22\n",
                ":3: hosts.d: unknown key 'port'",
            ),
            (
                "hosts:\n  d:\n    - host: me@a\n",
                ":3: hosts.d: host: bad host name",
            ),
            (
                "hosts:\n  d:\n    - host: a\n      reachability_check: 1\n",
                "reachability_check: expected true or false, found '1' (line 4)",
            ),
            ("hosts:\n  d: []\n", ":2: hosts.d: no hosts listed"),
            (
                "hosts:\n  a@b:\n    - host: x\n",
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
