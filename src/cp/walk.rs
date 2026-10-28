//! The local tree a sender walks (ACSCP_DESIGN §5.4, §6): its entries in
//! the order they go on the wire, with what `-p` carries of each, and the
//! symlink and loop rules.
//!
//! - Symlinks are followed, as `scp` does; a dangling one is skipped.
//! - Devices, sockets and fifos are skipped with a reason, and are never
//!   opened: a fifo would block the open.
//! - A directory is entered only with `-r`, and never twice on one path:
//!   the `(dev, ino)` of each directory on the way down is kept, so a
//!   symlink back to an ancestor is skipped rather than followed for ever.
//! - Entries come sorted by name, so a copy is the same every time.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::proto::{Meta, Time, MAX_NAME};

/// The most directories deep a walk goes, as the receiver accepts.
pub const MAX_DEPTH: usize = 256;

/// One step of the walk.
#[derive(Debug)]
pub enum Entry {
    /// Enter a directory.
    Dir { name: String, meta: Meta },
    /// Leave the directory last entered.
    DirEnd,
    /// A regular file, open for reading, with its size when opened.
    File {
        name: String,
        path: PathBuf,
        file: File,
        size: u64,
        meta: Meta,
    },
    /// Something that is not copied, and why. `path` is for the report.
    Skip { path: String, reason: String },
}

/// What `-p` takes of `m`.
pub fn meta_of(m: &std::fs::Metadata) -> Meta {
    let time = |secs: i64, nanos: i64| Time {
        secs,
        nanos: nanos.clamp(0, 999_999_999) as u32,
    };
    Meta {
        mode: m.mode() & 0o7777,
        mtime: time(m.mtime(), m.mtime_nsec()),
        atime: time(m.atime(), m.atime_nsec()),
    }
}

/// The name a source is copied under: its last component, or — for `.`,
/// `..` and the like — the last component of what it resolves to. `None`
/// when there is none (`/`) or it is not UTF-8.
pub fn top_name(path: &Path) -> Option<String> {
    let name = match path.file_name() {
        Some(n) => n.to_os_string(),
        None => std::fs::canonicalize(path)
            .ok()?
            .file_name()?
            .to_os_string(),
    };
    name.into_string().ok().filter(|n| n.len() <= MAX_NAME)
}

/// What an I/O error says, without the `(os error N)` the standard library
/// appends.
pub fn reason(e: &std::io::Error) -> String {
    // The suffix holds the last `(`: a scan for that one byte, not a search
    // for the whole suffix, which would bring a substring searcher of its
    // own into the binary.
    let mut s = e.to_string();
    if let Some(i) = s.bytes().rposition(|b| b == b'(') {
        if s[..i].ends_with(' ') && s[i..].starts_with("(os error ") {
            s.truncate(i - 1);
        }
    }
    s
}

/// The checks a local source passes before a copy starts (ACSCP_DESIGN
/// §4.2): it exists, it can be read, it is a directory only with `-r`, and
/// it has a name to be copied under. The name, or why not.
pub fn check_source(path: &Path, recursive: bool) -> Result<String, String> {
    let m = std::fs::metadata(path).map_err(|e| reason(&e))?;
    if m.is_dir() {
        if !recursive {
            return Err("is a directory (use -r)".into());
        }
        std::fs::read_dir(path).map_err(|e| reason(&e))?;
    } else if m.is_file() {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .map_err(|e| reason(&e))?;
    } else {
        return Err("not a regular file".into());
    }
    top_name(path).ok_or_else(|| "has no name to copy it under that is UTF-8".into())
}

/// The names in directory `path`, in byte order, as `OsString`'s own
/// `sort` puts them.
///
/// Sorted as [`sort_key`]s: the binary keeps the one `Vec<String>` sort it
/// already has rather than gain a second, some 3 KB, for `OsString`
/// (DESIGN §8.1).
pub(super) fn sorted_names(path: &Path) -> std::io::Result<Vec<OsString>> {
    let mut keys = Vec::new();
    for e in std::fs::read_dir(path)? {
        keys.push(sort_key(&e?.file_name()));
    }
    keys.sort();
    Ok(keys.iter().map(|k| name_of_key(k)).collect())
}

/// `name` as a `String` that sorts as it does: each byte one `char` of the
/// same value. Code point order is byte order, so the `String` order is the
/// name's, UTF-8 or not.
fn sort_key(name: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().iter().map(|&b| b as char).collect()
}

/// The name [`sort_key`] made `key` of.
fn name_of_key(key: &str) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(key.chars().map(|c| c as u8).collect())
}

struct Level {
    path: PathBuf,
    entries: std::vec::IntoIter<OsString>,
    id: (u64, u64),
}

/// The walk over a list of sources, each with the name it is sent under.
pub struct Walk {
    tops: std::vec::IntoIter<(String, PathBuf)>,
    stack: Vec<Level>,
    recursive: bool,
}

impl Walk {
    pub fn new(sources: Vec<(String, PathBuf)>, recursive: bool) -> Walk {
        Walk {
            tops: sources.into_iter(),
            stack: Vec::new(),
            recursive,
        }
    }

    /// The entry at `path`, to be sent as `name`.
    fn visit(&mut self, name: String, path: PathBuf) -> Entry {
        let skip = |reason: String| Entry::Skip {
            path: path.display().to_string(),
            reason,
        };
        let m = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let dangling = std::fs::symlink_metadata(&path).is_ok_and(|l| l.is_symlink());
                return skip(match dangling {
                    true => "dangling symlink".into(),
                    false => reason(&e),
                });
            }
        };
        if m.is_dir() {
            if !self.recursive {
                return skip("is a directory (use -r)".into());
            }
            if self.stack.len() >= MAX_DEPTH {
                return skip("too deep".into());
            }
            let id = (m.dev(), m.ino());
            if self.stack.iter().any(|l| l.id == id) {
                return skip("directory loop: already being copied".into());
            }
            let entries = match sorted_names(&path) {
                Ok(v) => v,
                Err(e) => return skip(reason(&e)),
            };
            self.stack.push(Level {
                path,
                entries: entries.into_iter(),
                id,
            });
            return Entry::Dir {
                name,
                meta: meta_of(&m),
            };
        }
        if !m.is_file() {
            return skip("not a regular file".into());
        }
        // Non-blocking, so something swapped in for the file after the
        // stat — a fifo — cannot hang the open; the handle is checked
        // again below.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => return skip(reason(&e)),
        };
        let m = match file.metadata() {
            Ok(m) if m.is_file() => m,
            Ok(_) => return skip("not a regular file".into()),
            Err(e) => return skip(reason(&e)),
        };
        Entry::File {
            name,
            path,
            file,
            size: m.len(),
            meta: meta_of(&m),
        }
    }
}

impl Iterator for Walk {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        let Some(level) = self.stack.last_mut() else {
            let (name, path) = self.tops.next()?;
            return Some(self.visit(name, path));
        };
        let Some(os) = level.entries.next() else {
            self.stack.pop();
            return Some(Entry::DirEnd);
        };
        let path = level.path.join(&os);
        match os.into_string() {
            Ok(name) if name.len() <= MAX_NAME => Some(self.visit(name, path)),
            _ => Some(Entry::Skip {
                path: path.display().to_string(),
                reason: "name is not UTF-8".into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn kinds(w: Walk) -> Vec<String> {
        w.map(|e| match e {
            Entry::Dir { name, .. } => format!("dir {name}"),
            Entry::DirEnd => "end".into(),
            Entry::File { name, size, .. } => format!("file {name} {size}"),
            Entry::Skip { path, reason } => {
                let base = Path::new(&path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                format!("skip {base}: {reason}")
            }
        })
        .collect()
    }

    /// Names are sorted through `String` keys to share the binary's one
    /// string sort (acs-hofu.10): the order is still `OsString`'s, byte by
    /// byte, for names that are not UTF-8 and for ones that are but sort
    /// differently as `char`s than as bytes would if the keys were wrong.
    #[test]
    fn names_sort_in_byte_order() {
        use std::os::unix::ffi::OsStringExt;
        let raw: [&[u8]; 12] = [
            b"b",
            b"a",
            b"\xff",
            b"\xc3\xa9",
            b"Z",
            b"a\x80",
            b"a",
            b"\x7f",
            b"\xc2\x80",
            b"",
            b"\xe2\x82\xac",
            b"ab",
        ];
        let names: Vec<OsString> = raw.iter().map(|b| OsString::from_vec(b.to_vec())).collect();
        let mut keys: Vec<String> = names.iter().map(|n| sort_key(n)).collect();
        keys.sort();
        let sorted: Vec<OsString> = keys.iter().map(|k| name_of_key(k)).collect();
        let mut want = names.clone();
        want.sort();
        assert_eq!(sorted, want);
    }

    /// The reason drops std's `(os error N)` and nothing else.
    #[test]
    fn a_reason_has_no_os_error_suffix() {
        let e = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert!(e
            .to_string()
            .ends_with(&format!(" (os error {})", libc::ENOENT)));
        assert_eq!(reason(&e), "No such file or directory");
        let e = std::io::Error::other("a (custom) one");
        assert_eq!(reason(&e), "a (custom) one");
        let e = std::io::Error::other("ends (os error 2)");
        assert_eq!(reason(&e), "ends");
    }

    #[test]
    fn a_tree_is_walked_sorted_with_its_ends() {
        let t = TempDir::new();
        let d = t.path().join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("b"), "bb").unwrap();
        std::fs::write(d.join("a"), "a").unwrap();
        std::fs::write(d.join("sub/c"), "").unwrap();
        let w = Walk::new(vec![("d".into(), d)], true);
        assert_eq!(
            kinds(w),
            ["dir d", "file a 1", "file b 2", "dir sub", "file c 0", "end", "end"]
        );
    }

    #[test]
    fn symlinks_are_followed_and_loops_and_dangling_ones_skipped() {
        let t = TempDir::new();
        let d = t.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(t.path().join("real"), "xyz").unwrap();
        std::os::unix::fs::symlink(t.path().join("real"), d.join("link")).unwrap();
        std::os::unix::fs::symlink(t.path().join("nowhere"), d.join("dangling")).unwrap();
        std::os::unix::fs::symlink("..", d.join("up")).unwrap();
        let w = Walk::new(vec![("d".into(), d)], true);
        let got = kinds(w);
        assert_eq!(got[0], "dir d");
        assert_eq!(got[1], "skip dangling: dangling symlink");
        assert_eq!(got[2], "file link 3");
        // `up` is the temp dir, which holds `d` again: entered once, and
        // `d` inside it refused as a loop.
        assert_eq!(got[3], "dir up");
        assert!(
            got.contains(&"skip d: directory loop: already being copied".to_string()),
            "{got:?}"
        );
        assert_eq!(got.last().unwrap(), "end");
    }

    #[test]
    fn directories_need_r_and_specials_are_skipped() {
        let t = TempDir::new();
        let d = t.path().join("d");
        std::fs::create_dir(&d).unwrap();
        let fifo = t.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let w = Walk::new(vec![("d".into(), d), ("fifo".into(), fifo)], false);
        assert_eq!(
            kinds(w),
            [
                "skip d: is a directory (use -r)",
                "skip fifo: not a regular file"
            ]
        );
    }

    #[test]
    fn the_top_name_is_the_last_component_or_what_it_resolves_to() {
        let t = TempDir::new();
        std::fs::create_dir(t.path().join("dir")).unwrap();
        assert_eq!(top_name(Path::new("a/b.txt")).as_deref(), Some("b.txt"));
        assert_eq!(top_name(&t.path().join("dir/")).as_deref(), Some("dir"));
        assert_eq!(top_name(&t.path().join("dir/.")).as_deref(), Some("dir"));
        let up = top_name(&t.path().join("dir/..")).unwrap();
        assert_eq!(
            Some(up.as_str()),
            t.path()
                .canonicalize()
                .unwrap()
                .file_name()
                .and_then(|n| n.to_str())
        );
        assert_eq!(top_name(Path::new("/")), None);
    }
}
