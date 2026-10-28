//! Which entry of an alias was used last, per set of local networks
//! (DESIGN §7.3, acs-zpj), so that the next connection from the same
//! networks takes it as soon as it answers its own ping instead of waiting
//! out the deadline for a higher-ranked host that is down there.
//!
//! State: `$XDG_STATE_HOME/acs/aliases` (default
//! `~/.local/state/acs/aliases`), one line per memory, newest first:
//! `<alias> TAB <host> TAB <network>,<network>,…`. It is a hint and nothing
//! more: a line that does not parse is dropped, a file that cannot be read
//! is empty, one that cannot be written stays as it was, and a host that is
//! no longer one of the alias's entries is ignored — so whatever the file
//! says, only a configured entry can ever be chosen through it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::netmatch::LocalNet;

/// How many memories the file keeps: one per alias and place, the oldest
/// written dropped first.
const KEEP: usize = 64;

/// Threads of one process (`acs list` resolves every alias at once) read,
/// change and write the file one at a time. Between processes the last
/// write wins, which loses at most a hint.
static FILE: Mutex<()> = Mutex::new(());

/// Where the memories are kept, and the networks this machine is on now —
/// the place a memory belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    path: PathBuf,
    /// Sorted, without duplicates, each in network form (`192.168.1.0/24`),
    /// so a new lease on the same subnet is the same place. Never empty.
    networks: Vec<String>,
}

impl Memory {
    /// A memory at `path` for a machine on `networks`; `None` when it is on
    /// no usable network at all, which is no place to remember anything for.
    pub fn new(path: PathBuf, networks: &[LocalNet]) -> Option<Memory> {
        let mut networks: Vec<String> = networks
            .iter()
            .filter(|n| n.usable())
            .map(|n| n.to_string())
            .collect();
        networks.sort();
        networks.dedup();
        (!networks.is_empty()).then_some(Memory { path, networks })
    }

    /// The state file and this machine's networks: `getifaddrs`, or the
    /// file `ACS_NETWATCH_NETS` names (one CIDR per line), as the network
    /// watcher reads them, so a test can say where the machine is.
    pub fn system() -> Option<Memory> {
        let nets = match std::env::var_os("ACS_NETWATCH_NETS") {
            Some(path) => std::fs::read_to_string(path)
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|s| LocalNet::parse(s).ok())
                .collect(),
            None => crate::netmatch::local_networks(),
        };
        Memory::new(state_path()?, &nets)
    }

    /// The file the memories are kept in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The host last used for `alias` on these networks, if any.
    pub fn recall(&self, alias: &str) -> Option<String> {
        let _held = FILE.lock().unwrap_or_else(|e| e.into_inner());
        read(&self.path)
            .into_iter()
            .find(|l| l.alias == alias && l.networks == self.networks)
            .map(|l| l.host)
    }

    /// Remember `host` for `alias` on these networks, or forget what was
    /// there (`None`). Nothing is written when nothing changes.
    pub fn remember(&self, alias: &str, host: Option<&str>) -> std::io::Result<()> {
        let _held = FILE.lock().unwrap_or_else(|e| e.into_inner());
        let mut lines = read(&self.path);
        let at = lines
            .iter()
            .position(|l| l.alias == alias && l.networks == self.networks);
        match (at, host) {
            (Some(i), Some(h)) if lines[i].host == h => return Ok(()),
            (None, None) => return Ok(()),
            (Some(i), _) => {
                lines.remove(i);
            }
            (None, Some(_)) => {}
        }
        if let Some(h) = host {
            lines.insert(
                0,
                Line {
                    alias: alias.to_string(),
                    host: h.to_string(),
                    networks: self.networks.clone(),
                },
            );
        }
        lines.truncate(KEEP);
        write(&self.path, &lines)
    }
}

/// `$XDG_STATE_HOME/acs/aliases`, or `~/.local/state/acs/aliases`.
pub fn state_path() -> Option<PathBuf> {
    state_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("acs/aliases"))
}

/// One memory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    alias: String,
    host: String,
    networks: Vec<String>,
}

/// The lines of `text` that parse; the rest are dropped.
fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (alias, host, nets) = (f.next()?, f.next()?, f.next()?);
            if f.next().is_some() || alias.is_empty() || host.is_empty() {
                return None;
            }
            let mut networks: Vec<String> = Vec::new();
            for n in nets.split(',') {
                networks.push(LocalNet::parse(n).ok()?.to_string());
            }
            networks.sort();
            networks.dedup();
            Some(Line {
                alias: alias.to_string(),
                host: host.to_string(),
                networks,
            })
        })
        .collect()
}

fn format(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| format!("{}\t{}\t{}\n", l.alias, l.host, l.networks.join(",")))
        .collect()
}

/// The memories at `path`: none when it is missing or unreadable.
fn read(path: &Path) -> Vec<Line> {
    std::fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Write the memories atomically: the file is replaced whole or not at all.
fn write(path: &Path, lines: &[Line]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    std::fs::create_dir_all(dir)?;
    // Which hosts the user reaches from which networks: ours alone to
    // read, whatever the umask (acs-g4uc).
    crate::sys::write_atomic(path, 0o600, format(lines).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn net(s: &str) -> LocalNet {
        LocalNet::parse(s).unwrap()
    }

    fn at(dir: &TempDir, nets: &[&str]) -> Memory {
        let nets: Vec<LocalNet> = nets.iter().map(|s| net(s)).collect();
        Memory::new(dir.path().join("acs/aliases"), &nets).unwrap()
    }

    #[test]
    fn a_memory_belongs_to_its_alias_and_its_networks() {
        let dir = TempDir::new();
        let home = at(&dir, &["192.168.1.5/24", "fd00:1::5/64"]);
        assert_eq!(home.recall("devbox"), None);
        home.remember("devbox", Some("devbox.lan")).unwrap();
        assert_eq!(home.recall("devbox").as_deref(), Some("devbox.lan"));
        assert_eq!(home.recall("nas"), None);
        // The same networks in another order, from another address on
        // them: the same place.
        let again = at(&dir, &["fd00:1::77/64", "192.168.1.9/24"]);
        assert_eq!(again.recall("devbox").as_deref(), Some("devbox.lan"));
        // One network more or less is another place.
        assert_eq!(at(&dir, &["192.168.1.5/24"]).recall("devbox"), None);
        let vpn = at(&dir, &["192.168.1.5/24", "fd00:1::5/64", "10.8.0.2/24"]);
        assert_eq!(vpn.recall("devbox"), None);
        vpn.remember("devbox", Some("devbox.vpn")).unwrap();
        assert_eq!(vpn.recall("devbox").as_deref(), Some("devbox.vpn"));
        assert_eq!(home.recall("devbox").as_deref(), Some("devbox.lan"));
        // Forgetting is for this place only.
        home.remember("devbox", None).unwrap();
        assert_eq!(home.recall("devbox"), None);
        assert_eq!(vpn.recall("devbox").as_deref(), Some("devbox.vpn"));
    }

    #[test]
    fn no_usable_network_is_no_place() {
        let dir = TempDir::new();
        let p = dir.path().join("aliases");
        assert_eq!(Memory::new(p.clone(), &[]), None);
        assert_eq!(
            Memory::new(p, &[LocalNet::new("127.0.0.1".parse().unwrap(), 8)]),
            None
        );
    }

    #[test]
    fn a_change_rewrites_one_line_newest_first_and_nothing_else_writes() {
        let dir = TempDir::new();
        let m = at(&dir, &["192.168.1.5/24"]);
        m.remember("a", Some("a1")).unwrap();
        m.remember("b", Some("b1")).unwrap();
        m.remember("a", Some("a2")).unwrap();
        let text = std::fs::read_to_string(m.path()).unwrap();
        assert_eq!(text, "a\ta2\t192.168.1.0/24\nb\tb1\t192.168.1.0/24\n");
        // Root writes through any mode, so the rest says nothing there.
        if crate::sys::getuid() == 0 {
            return;
        }
        // The same again changes nothing, so nothing is written: a
        // directory made read-only would make a write fail.
        use std::os::unix::fs::PermissionsExt;
        let ro = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(m.path().parent().unwrap(), ro).unwrap();
        assert!(m.remember("a", Some("a2")).is_ok());
        assert!(m.remember("c", None).is_ok());
        // A real change fails, and leaves the file as it was.
        assert!(m.remember("a", Some("a3")).is_err());
        assert_eq!(m.recall("a").as_deref(), Some("a2"));
        let rw = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(m.path().parent().unwrap(), rw).unwrap();
    }

    #[test]
    fn the_file_keeps_the_newest_memories() {
        let dir = TempDir::new();
        let m = at(&dir, &["192.168.1.5/24"]);
        for i in 0..KEEP + 5 {
            m.remember(&format!("alias{i}"), Some("h")).unwrap();
        }
        let text = std::fs::read_to_string(m.path()).unwrap();
        assert_eq!(text.lines().count(), KEEP);
        assert_eq!(
            m.recall(&format!("alias{}", KEEP + 4)).as_deref(),
            Some("h")
        );
        assert_eq!(m.recall("alias4"), None);
        assert_eq!(m.recall("alias5").as_deref(), Some("h"));
    }

    #[test]
    fn a_corrupt_file_is_what_parses_of_it_and_is_mended_by_a_write() {
        let dir = TempDir::new();
        let m = at(&dir, &["192.168.1.5/24"]);
        std::fs::create_dir_all(m.path().parent().unwrap()).unwrap();
        std::fs::write(
            m.path(),
            "garbage\n\
             a\tgood\t192.168.1.0/24\n\
             b\thost\tnot-a-network\n\
             c\thost\t192.168.1.0/24\textra\n\
             \thost\t192.168.1.0/24\n",
        )
        .unwrap();
        assert_eq!(m.recall("a").as_deref(), Some("good"));
        assert_eq!(m.recall("b"), None);
        assert_eq!(m.recall("c"), None);
        m.remember("d", Some("x")).unwrap();
        assert_eq!(
            std::fs::read_to_string(m.path()).unwrap(),
            "d\tx\t192.168.1.0/24\na\tgood\t192.168.1.0/24\n"
        );
        // Not even text: nothing recalled, and a write replaces it.
        std::fs::write(m.path(), [0xff, 0xfe, 0x00, 0x80]).unwrap();
        assert_eq!(m.recall("a"), None);
        m.remember("a", Some("y")).unwrap();
        assert_eq!(m.recall("a").as_deref(), Some("y"));
    }

    #[test]
    fn the_state_lives_under_xdg_state_home() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            state_path_from(os("/st"), os("/home/me")),
            Some(PathBuf::from("/st/acs/aliases"))
        );
        // A relative one is ignored, as the spec says.
        assert_eq!(
            state_path_from(os("rel"), os("/home/me")),
            Some(PathBuf::from("/home/me/.local/state/acs/aliases"))
        );
        assert_eq!(state_path_from(None, os("")), None);
    }
}
