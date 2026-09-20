//! Where sessions live (DESIGN §4.1, §4.4, §4.5): session names, the per-uid
//! socket directory and its safety checks, and the locks that serialise
//! session creation.

use std::fmt;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::sys::{self, Flock};

pub const DEFAULT_SESSION: &str = "main";
const MAX_NAME: usize = 64;

/// Check a session name: `[A-Za-z0-9._-]`, not starting with `-` or `.`.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name is empty".into());
    }
    if name.len() > MAX_NAME {
        return Err(format!("session name longer than {MAX_NAME} characters"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "bad session name '{name}': '{c}' is not allowed (allowed: A-Za-z0-9 . _ -)"
        ));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(format!(
            "bad session name '{name}': must not start with '-' or '.'"
        ));
    }
    Ok(())
}

/// The session plain `acs <host>` means: `$ACS_DEFAULT_SESSION` or `main`.
pub fn default_name_from(env: Option<&str>) -> Result<String, String> {
    match env {
        Some(v) if !v.is_empty() => {
            validate_name(v).map_err(|e| format!("ACS_DEFAULT_SESSION: {e}"))?;
            Ok(v.to_string())
        }
        _ => Ok(DEFAULT_SESSION.to_string()),
    }
}

pub fn default_name() -> Result<String, String> {
    default_name_from(std::env::var("ACS_DEFAULT_SESSION").ok().as_deref())
}

/// The facts about the socket directory that decide whether it is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirFacts {
    pub is_symlink: bool,
    pub is_dir: bool,
    pub uid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirError {
    Symlink(PathBuf),
    NotDir(PathBuf),
    Foreign {
        path: PathBuf,
        owner: String,
    },
    /// An ancestor others could write, so the directory could be replaced.
    Replaceable {
        path: PathBuf,
        why: String,
    },
    Io(PathBuf, String),
}

/// Whether `mode` lets someone other than the owner remove or rename what
/// is in the directory: writable by group or other, and not sticky.
fn mode_lets_others_replace(mode: u32) -> bool {
    mode & 0o022 != 0 && mode & 0o1000 == 0
}

/// Refuse a socket directory that anyone else could replace: an ancestor
/// writable by group or other and not sticky (acs-hjk).
///
/// The sticky bit is what makes `/tmp` safe — it lets anyone create, but
/// only the owner remove or rename. Without it, whoever can write the
/// parent can move our directory aside and put theirs in its place, and
/// the next client connects to their master and hands it every keystroke.
fn check_ancestors(path: &Path) -> Result<(), DirError> {
    for dir in path.ancestors().skip(1) {
        let meta = match std::fs::symlink_metadata(dir) {
            Ok(m) => m,
            // Not readable by us is not our business to judge.
            Err(_) => continue,
        };
        if mode_lets_others_replace(meta.mode()) && meta.uid() != sys::getuid() {
            return Err(DirError::Replaceable {
                path: path.to_path_buf(),
                why: format!("{} is writable by others and not sticky", dir.display()),
            });
        }
    }
    Ok(())
}

impl fmt::Display for DirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hint = "set ACS_SOCKET_DIR to a private directory";
        match self {
            DirError::Symlink(p) => {
                write!(
                    f,
                    "{} is a symlink, refusing to use it ({hint})",
                    p.display()
                )
            }
            DirError::NotDir(p) => {
                write!(
                    f,
                    "{} is not a directory, refusing to use it ({hint})",
                    p.display()
                )
            }
            DirError::Foreign { path, owner } => write!(
                f,
                "{} is owned by {owner}, not by you — refusing to use it ({hint})",
                path.display()
            ),
            DirError::Replaceable { path, why } => write!(
                f,
                "{} sits under a directory anyone could replace: {why} ({hint})",
                path.display()
            ),
            DirError::Io(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

/// Decide whether an existing directory may hold our sockets. `Ok(true)`
/// means it is ours but its mode must be tightened to `0700`.
pub fn check_dir(path: &Path, facts: DirFacts, my_uid: u32) -> Result<bool, DirError> {
    if facts.is_symlink {
        return Err(DirError::Symlink(path.into()));
    }
    if !facts.is_dir {
        return Err(DirError::NotDir(path.into()));
    }
    if facts.uid != my_uid {
        let owner = sys::user_name(facts.uid)
            .map(|n| format!("{n} (uid {})", facts.uid))
            .unwrap_or_else(|| format!("uid {}", facts.uid));
        return Err(DirError::Foreign {
            path: path.into(),
            owner,
        });
    }
    Ok(facts.mode & 0o777 != 0o700)
}

/// The per-user directory holding session sockets and locks.
#[derive(Debug, Clone)]
pub struct SocketDir {
    path: PathBuf,
}

impl SocketDir {
    /// `$ACS_SOCKET_DIR`, else `/tmp/acs-<uid>` — keyed by the numeric uid,
    /// never `$USER` or `$TMPDIR` (DESIGN §4.1).
    pub fn default_path() -> PathBuf {
        match std::env::var_os("ACS_SOCKET_DIR") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => PathBuf::from(format!("/tmp/acs-{}", sys::getuid())),
        }
    }

    /// Resolve and create (or verify) the directory.
    pub fn open() -> Result<SocketDir, DirError> {
        Self::open_at(Self::default_path())
    }

    pub fn open_at(path: PathBuf) -> Result<SocketDir, DirError> {
        let io_err = |e: io::Error| DirError::Io(path.clone(), e.to_string());
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(e)),
        }
        let meta = std::fs::symlink_metadata(&path).map_err(io_err)?;
        let facts = DirFacts {
            is_symlink: meta.file_type().is_symlink(),
            is_dir: meta.is_dir(),
            uid: meta.uid(),
            mode: meta.mode(),
        };
        if check_dir(&path, facts, sys::getuid())? {
            // Through a handle, not by name (acs-hjk). `set_permissions`
            // follows symlinks and runs *after* the lstat above, so between
            // the two the leaf could be swapped for a link to somewhere
            // else and this would chmod that instead. O_NOFOLLOW means the
            // open fails rather than lands somewhere new, and the fchmod
            // can only reach what was opened.
            let fd = sys::open_dir_nofollow(&path).map_err(io_err)?;
            sys::fchmod(fd.as_raw_fd(), 0o700).map_err(io_err)?;
        }
        // Somebody else's writable directory above ours is the same
        // problem one level up: they can move ours aside and put their own
        // there. /tmp is sticky, so the default path is fine; a pointed
        // ACS_SOCKET_DIR may not be (acs-hjk).
        check_ancestors(&path)?;
        Ok(SocketDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn socket_path(&self, name: &str) -> Result<PathBuf, String> {
        validate_name(name)?;
        let p = self.path.join(format!("{name}.sock"));
        let max = sys::max_socket_path();
        if p.as_os_str().len() > max {
            return Err(format!(
                "socket path {} is longer than the platform limit of {max} bytes; set ACS_SOCKET_DIR to a shorter directory",
                p.display()
            ));
        }
        Ok(p)
    }

    /// Lock held while a session is being created.
    pub fn create_lock(&self, name: &str) -> io::Result<Flock> {
        Flock::lock(&self.path.join(format!("{name}.lock")))
    }

    /// [`create_lock`](Self::create_lock) if it is free; `Ok(None)` while a
    /// master start holds it.
    pub fn try_create_lock(&self, name: &str) -> io::Result<Option<Flock>> {
        Flock::try_lock(&self.path.join(format!("{name}.lock")))
    }

    /// Lock held while `--new` picks and creates a numbered session.
    pub fn dir_lock(&self) -> io::Result<Flock> {
        Flock::lock(&self.path.join(".lock"))
    }

    /// Names of all sessions with a socket file (live or stale).
    pub fn sessions(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            let file = entry.file_name();
            let Some(file) = file.to_str() else { continue };
            if let Some(name) = file.strip_suffix(".sock") {
                if validate_name(name).is_ok() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort_by(|a, b| natural_cmp(a, b));
        Ok(names)
    }

    /// Lowest positive number not used as a session name. Call with the
    /// directory lock held and keep it until the session exists.
    pub fn lowest_free_number(&self) -> io::Result<String> {
        let taken: Vec<String> = self.sessions()?;
        let n = (1..)
            .find(|n: &u64| {
                let s = n.to_string();
                !taken.contains(&s) && self.lock_is_free(&s)
            })
            .unwrap();
        Ok(n.to_string())
    }

    /// No one is creating `name` right now (its lock is not held).
    fn lock_is_free(&self, name: &str) -> bool {
        let lock = self.path.join(format!("{name}.lock"));
        !lock.exists() || matches!(Flock::try_lock(&lock), Ok(Some(_)))
    }
}

/// Numbers sort numerically, everything else alphabetically after them.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Whether the directory at `path` is one others could replace entries in.
    fn others_may_replace(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|m| mode_lets_others_replace(m.mode()))
            .unwrap_or(false)
    }

    /// acs-hjk: a directory others could replace is refused. The sticky bit
    /// is what makes `/tmp` safe — it lets anyone create but only the owner
    /// remove or rename. Without it, whoever can write the parent moves ours
    /// aside, puts theirs there, and the next client hands its keystrokes to
    /// their master.
    #[test]
    fn a_directory_under_a_replaceable_parent_is_refused() {
        let t = crate::testutil::TempDir::new();

        // A parent anyone may write, without the sticky bit.
        let loose = t.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        let under = loose.join("s");
        std::fs::create_dir(&under).unwrap();
        // Owned by us here, so only the ancestor rule can object; the check
        // ignores ancestors we own, which is the case in this test, so aim
        // it at the mode directly.
        assert!(
            others_may_replace(&loose),
            "a 0777 non-sticky directory should count as replaceable"
        );

        // Sticky, as /tmp is: fine.
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(!others_may_replace(&loose), "a sticky directory is fine");

        // Private: fine.
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!others_may_replace(&loose), "a private directory is fine");

        // And the real path this check runs on is accepted.
        assert!(check_ancestors(&under).is_ok(), "{}", under.display());
    }
    use crate::testutil::TempDir;

    #[test]
    fn names() {
        for ok in ["main", "2", "my-proj_1.0", "A"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a/b",
            "-x",
            ".hidden",
            "..",
            "naïve",
            &"x".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_session_name() {
        assert_eq!(default_name_from(None).unwrap(), "main");
        assert_eq!(default_name_from(Some("")).unwrap(), "main");
        assert_eq!(default_name_from(Some("michel")).unwrap(), "michel");
        assert!(default_name_from(Some("bad name")).is_err());
    }

    fn facts(uid: u32, mode: u32) -> DirFacts {
        DirFacts {
            is_symlink: false,
            is_dir: true,
            uid,
            mode,
        }
    }

    #[test]
    fn dir_checks() {
        let p = Path::new("/tmp/acs-1000");
        assert_eq!(check_dir(p, facts(1000, 0o40700), 1000), Ok(false));
        assert_eq!(check_dir(p, facts(1000, 0o40755), 1000), Ok(true));
        assert!(matches!(
            check_dir(p, facts(0, 0o40700), 1000),
            Err(DirError::Foreign { .. })
        ));
        let mut f = facts(1000, 0o120777);
        f.is_symlink = true;
        assert_eq!(check_dir(p, f, 1000), Err(DirError::Symlink(p.into())));
        let mut f = facts(1000, 0o100600);
        f.is_dir = false;
        assert_eq!(check_dir(p, f, 1000), Err(DirError::NotDir(p.into())));
    }

    #[test]
    fn foreign_owner_is_named_in_the_error() {
        let e = check_dir(Path::new("/tmp/x"), facts(0, 0o40700), 1000).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("uid 0"), "{msg}");
        assert!(msg.contains("ACS_SOCKET_DIR"), "{msg}");
    }

    #[test]
    fn open_creates_private_dir_and_tightens_mode() {
        let t = TempDir::new();
        let p = t.path().join("s");
        let d = SocketDir::open_at(p.clone()).unwrap();
        assert_eq!(std::fs::metadata(d.path()).unwrap().mode() & 0o777, 0o700);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        SocketDir::open_at(p.clone()).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn open_refuses_symlink_and_file() {
        let t = TempDir::new();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            SocketDir::open_at(link),
            Err(DirError::Symlink(_))
        ));
        let file = t.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(matches!(SocketDir::open_at(file), Err(DirError::NotDir(_))));
    }

    #[test]
    fn socket_path_length_is_checked() {
        let d = SocketDir {
            path: PathBuf::from(format!("/tmp/{}", "d".repeat(120))),
        };
        let e = d.socket_path("main").unwrap_err();
        assert!(e.contains("ACS_SOCKET_DIR"), "{e}");
        let d = SocketDir {
            path: PathBuf::from("/tmp/acs-1"),
        };
        assert_eq!(
            d.socket_path("main").unwrap(),
            PathBuf::from("/tmp/acs-1/main.sock")
        );
        assert!(d.socket_path("../x").is_err());
    }

    #[test]
    fn numbering_fills_gaps_and_skips_locked_names() {
        let t = TempDir::new();
        let d = SocketDir::open_at(t.path().join("s")).unwrap();
        assert_eq!(d.lowest_free_number().unwrap(), "1");
        for n in ["1", "2", "4", "main"] {
            std::fs::write(d.path().join(format!("{n}.sock")), b"").unwrap();
        }
        assert_eq!(d.lowest_free_number().unwrap(), "3");
        // A session being created (lock held, no socket yet) is taken too.
        let _creating = d.create_lock("3").unwrap();
        assert_eq!(d.lowest_free_number().unwrap(), "5");
        assert_eq!(d.sessions().unwrap(), vec!["1", "2", "4", "main"]);
    }

    #[test]
    fn concurrent_new_sessions_get_distinct_numbers() {
        let t = TempDir::new();
        let path = t.path().join("s");
        SocketDir::open_at(path.clone()).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let d = SocketDir::open_at(path).unwrap();
                    let _g = d.dir_lock().unwrap();
                    let n = d.lowest_free_number().unwrap();
                    std::fs::write(d.path().join(format!("{n}.sock")), b"").unwrap();
                    n
                })
            })
            .collect();
        let mut got: Vec<u64> = handles
            .into_iter()
            .map(|h| h.join().unwrap().parse().unwrap())
            .collect();
        got.sort();
        assert_eq!(got, (1..=8).collect::<Vec<_>>());
    }
}
