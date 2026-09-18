//! Housekeeping of installed remote versions (DESIGN §8): each proxy marks
//! its own version directory as used; version directories untouched for 30
//! days, with no live session still running them, are removed — at most
//! once a day, at a proxy start.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long an unused version is kept (`ACS_PRUNE_AFTER_SECS` for tests).
fn max_age() -> Duration {
    Duration::from_secs(
        std::env::var("ACS_PRUNE_AFTER_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30 * 86_400),
    )
}

/// Minimum time between prune runs (`ACS_PRUNE_EVERY_SECS` for tests).
fn every() -> Duration {
    Duration::from_secs(
        std::env::var("ACS_PRUNE_EVERY_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(86_400),
    )
}

/// `~/.local/share/acs`, if this binary runs from a version directory in it.
pub fn versions_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    root_for(&exe, crate::VERSION, Path::new(&home))
}

/// The versions directory `exe` runs from, only if it is the user's own
/// `home/.local/share/acs`: a system-wide install (`/usr/local/lib/acs`)
/// holds other users' versions and is never touched (DESIGN §8).
fn root_for(exe: &Path, version: &str, home: &Path) -> Option<PathBuf> {
    let version_dir = exe.parent()?;
    if version_dir.file_name()?.to_str()? != version {
        return None;
    }
    let root = version_dir.parent()?;
    let own = home.join(".local/share/acs");
    let same = root == own
        || matches!(
            (std::fs::canonicalize(root), std::fs::canonicalize(&own)),
            (Ok(a), Ok(b)) if a == b
        );
    same.then(|| root.to_path_buf())
}

fn age(path: &Path, now: SystemTime) -> Option<Duration> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(m).ok().or(Some(Duration::ZERO))
}

/// Remove version directories under `root` other than `keep` that are older
/// than `max_age` and not in `live`. Returns what was removed.
pub fn prune(
    root: &Path,
    keep: &str,
    live: &HashSet<String>,
    max_age: Duration,
    now: SystemTime,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if name == keep || name.starts_with('.') || live.contains(&name) || !path.is_dir() {
            continue;
        }
        if age(&path, now).is_some_and(|a| a > max_age) && std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// Mark our version used and, if due, prune the others. `live_versions`
/// asks the running masters which versions they run; it is only called
/// when a prune is due.
pub fn on_proxy_start(live_versions: impl FnOnce() -> HashSet<String>) {
    let Some(root) = versions_root() else { return };
    let _ = crate::sys::touch(&root.join(crate::VERSION));
    let marker = root.join(".pruned");
    let now = SystemTime::now();
    if age(&marker, now).is_some_and(|a| a < every()) {
        return;
    }
    let _ = std::fs::write(&marker, b"");
    prune(&root, crate::VERSION, &live_versions(), max_age(), now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Regression (acs-rxw): a binary run from the system-wide
    /// `/usr/local/lib/acs/<v>/acs` prunes nothing.
    #[test]
    fn only_the_users_own_versions_directory_is_pruned() {
        let home = TempDir::new();
        let own = home.path().join(".local/share/acs/1.2.3/acs");
        assert_eq!(
            root_for(&own, "1.2.3", home.path()),
            Some(home.path().join(".local/share/acs"))
        );
        let system = Path::new("/usr/local/lib/acs/1.2.3/acs");
        assert_eq!(root_for(system, "1.2.3", home.path()), None);
        // Another user's HOME is not ours either.
        let other = Path::new("/home/someone/.local/share/acs/1.2.3/acs");
        assert_eq!(root_for(other, "1.2.3", home.path()), None);
        // Not from a version directory at all.
        assert_eq!(
            root_for(&home.path().join("acs"), "1.2.3", home.path()),
            None
        );
    }

    #[test]
    fn prunes_only_old_unused_other_versions() {
        let t = TempDir::new();
        let root = t.path();
        let now = SystemTime::now();
        let day = 86_400;
        for (v, days_old) in [("0.1.0", 90), ("0.2.0", 90), ("0.3.0", 90), ("0.4.0", 2)] {
            let d = root.join(v);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("acs"), b"bin").unwrap();
            crate::sys::set_mtime(&d, crate::sys::unix_now() - days_old * day).unwrap();
        }
        std::fs::write(root.join(".pruned"), b"").unwrap();
        let live: HashSet<String> = ["0.2.0".to_string()].into();
        let removed = prune(root, "0.3.0", &live, Duration::from_secs(30 * day), now);
        assert_eq!(removed, vec![root.join("0.1.0")]);
        for kept in ["0.2.0", "0.3.0", "0.4.0", ".pruned"] {
            assert!(root.join(kept).exists(), "{kept} must stay");
        }
    }

    #[test]
    fn missing_root_is_fine() {
        let removed = prune(
            Path::new("/nonexistent/acs-prune"),
            "x",
            &HashSet::new(),
            Duration::ZERO,
            SystemTime::now(),
        );
        assert!(removed.is_empty());
    }
}
