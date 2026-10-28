//! The receiving endpoint (ACSCP_DESIGN §5.4, §6): place what arrives by
//! `scp`'s rules, trusting nothing about a name.
//!
//! - **Names** are single components (checked as the frame is decoded),
//!   nesting is only by `DIR`/`DIR_END`, so the current directory is
//!   always inside the target, and at the top level only the names asked
//!   for are accepted — CVE-2019-6111's lesson.
//! - **Placement**: one source into anything but a directory becomes that
//!   path; otherwise each source goes inside the target, which must then
//!   be an existing directory (`ERROR` before any data if several sources
//!   were asked into something else).
//! - **Files** are written to `.<name>.acscp-part` beside their final name
//!   and renamed over it only when whole, so nothing ever sees half a file
//!   under the final name. A new part is made `O_EXCL` (a symlink planted
//!   at its name is refused, not written through); an existing one is
//!   reopened `O_NOFOLLOW` and used only if it is a regular file.
//! - **Directories** are made `0700 | mode`, so their contents can be
//!   written, and get their final mode and, with `-p`, their times once
//!   everything inside them is done — which, since the sender's headers
//!   run ahead of its data, may be after their `DIR_END`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::proto::{err, Hello, Meta, Msg, Role, Time};
use super::walk::{reason, MAX_DEPTH};
use super::{note_on, Endpoint, Halt, Options, Outcome};
use crate::outq::OutQueue;
use crate::sys;

/// A directory made or entered, until its final mode and times are set.
struct Dir {
    path: PathBuf,
    parent: Option<usize>,
    meta: Meta,
    /// Made by this copy, rather than found there.
    made: bool,
    /// Its `DIR_END` came.
    ended: bool,
    /// Files and directories inside it not yet done.
    open: usize,
}

/// Where the next entry goes.
#[derive(Clone, Copy)]
enum Level {
    Dir(usize),
    /// Inside a directory that was refused: nothing is written.
    Refused,
}

/// A file being written.
struct Part {
    file: File,
    part: PathBuf,
    dest: PathBuf,
    name: String,
    size: u64,
    written: u64,
    meta: Meta,
    dir: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Hello,
    /// A download's receiver, whose `HELLO` went to the remote sender:
    /// waiting for its `WELCOME`.
    Welcome,
    Running,
    /// `DONE` is queued; the copy is over once the sender closes.
    Done,
    Closed,
}

pub struct Receiver {
    state: State,
    target: PathBuf,
    target_is_dir: bool,
    names: Vec<String>,
    preserve: bool,
    umask: u32,
    dirs: Vec<Dir>,
    levels: Vec<Level>,
    /// The files by id: `Some` a part being written, `None` a file refused
    /// or failed, whose data may still be on the way. One map rather than
    /// a map and a set, which would be two copies of the table code.
    parts: HashMap<u32, Option<Part>>,
    report: bool,
    outcome: Outcome,
}

/// The process's umask, read without changing it for longer than the read.
pub(super) fn umask() -> u32 {
    // SAFETY: umask cannot fail; the old mask is put straight back.
    let mask = unsafe { libc::umask(0o077) };
    unsafe { libc::umask(mask) };
    mask as u32 & 0o777
}

/// `.<name>.acscp-part` beside `dest`: fixed rather than random, since it
/// is what a resume finds (ACSCP_DESIGN §7).
pub fn part_path(dest: &Path) -> Option<PathBuf> {
    let name = dest.file_name()?;
    let mut part = std::ffi::OsString::from(".");
    part.push(name);
    part.push(".acscp-part");
    Some(dest.with_file_name(part))
}

fn system_time(t: Time) -> SystemTime {
    let d = Duration::new(t.secs.unsigned_abs(), 0);
    let base = match t.secs >= 0 {
        true => SystemTime::UNIX_EPOCH + d,
        false => SystemTime::UNIX_EPOCH - d,
    };
    base + Duration::from_nanos(t.nanos as u64)
}

/// Set `f`'s times to `m`'s, with their sub-second part.
fn set_times(f: &File, m: &Meta) -> std::io::Result<()> {
    let t = std::fs::FileTimes::new()
        .set_accessed(system_time(m.atime))
        .set_modified(system_time(m.mtime));
    f.set_times(t)
}

/// Open the part at `part` for writing from its start, made at `mode`.
fn open_part(part: &Path, mode: u32) -> std::io::Result<File> {
    match sys::create_new_mode(part, mode) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        other => return other,
    }
    // A part from an earlier run. Only a regular file is used, and never
    // through a symlink. Until resuming exists (phase 3, HAVE) it is
    // written again from the start.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(part)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "a part file is there and is not a regular file",
        ));
    }
    f.set_len(0)?;
    sys::fchmod(f.as_raw_fd(), mode)?;
    Ok(f)
}

impl Receiver {
    /// A receiver that waits for its `HELLO` and answers it, as the remote
    /// end and a local copy's second half do.
    pub fn serving(opts: &Options) -> Receiver {
        Receiver {
            state: State::Hello,
            target: PathBuf::new(),
            target_is_dir: false,
            names: Vec::new(),
            preserve: opts.preserve,
            umask: umask(),
            dirs: Vec::new(),
            levels: Vec::new(),
            parts: HashMap::new(),
            report: opts.report,
            outcome: Outcome::default(),
        }
    }

    /// The receiver of a download: it asked the remote sender for its
    /// sources and accepts at the top level only `names`, theirs
    /// (ACSCP_DESIGN §5.4), written to `target` by the rules a served
    /// receiver follows. Several names need `target` to be a directory,
    /// which the caller has checked.
    pub fn asking(target: PathBuf, names: Vec<String>, opts: &Options) -> Receiver {
        let mut r = Receiver::serving(opts);
        r.state = State::Welcome;
        r.target_is_dir = target.is_dir();
        r.target = target;
        r.names = names;
        r
    }

    pub fn outcome(mut self, error: Option<String>) -> Outcome {
        // A part not finished is kept, under its part name: it is what a
        // later run resumes from (ACSCP_DESIGN §7).
        self.parts.clear();
        self.outcome.error = error;
        self.outcome
    }

    fn report(&self, what: &str, why: &str) {
        if self.report {
            note_on(what, why);
        }
    }

    fn hello(&mut self, h: Hello, out: &mut OutQueue) -> Result<(), Halt> {
        let refuse = |code, message: String, out: &mut OutQueue| {
            Msg::Error {
                code,
                message: message.clone(),
            }
            .encode(out.tail());
            Err(Halt::Ended(message))
        };
        // The version is the remote end's to check (`remote::serve`): a
        // local copy's two halves are one build.
        if h.role != Role::Receive {
            return refuse(
                err::REQUEST,
                "asked to send, but this end receives".into(),
                out,
            );
        }
        if h.paths.is_empty() {
            return refuse(err::REQUEST, "nothing to copy".into(), out);
        }
        self.target = PathBuf::from(if h.target.is_empty() { "." } else { &h.target });
        self.target_is_dir = std::fs::metadata(&self.target).is_ok_and(|m| m.is_dir());
        if h.paths.len() > 1 && !self.target_is_dir {
            let m = format!("{}: not a directory", self.target.display());
            return refuse(err::REQUEST, m, out);
        }
        self.names = h.paths;
        self.preserve = h.preserve;
        Msg::Welcome {
            umask: self.umask,
            target_is_dir: self.target_is_dir,
        }
        .encode(out.tail());
        self.state = State::Running;
        Ok(())
    }

    /// Where an entry named `name` goes at the current level, or `None`
    /// inside a refused directory. At the top level, `Err` is a name that
    /// was not asked for.
    fn place(&self, name: &str) -> Result<Option<(PathBuf, Option<usize>)>, String> {
        match self.levels.last() {
            Some(Level::Refused) => Ok(None),
            Some(Level::Dir(i)) => Ok(Some((self.dirs[*i].path.join(name), Some(*i)))),
            None if !self.names.iter().any(|n| n == name) => {
                Err("refused: not a name that was asked for".into())
            }
            None if self.target_is_dir => Ok(Some((self.target.join(name), None))),
            None => Ok(Some((self.target.clone(), None))),
        }
    }

    fn fail(&mut self, id: u32, name: &str, message: String, out: &mut OutQueue) {
        self.outcome.failed += 1;
        if id != 0 {
            self.parts.insert(id, None);
        }
        self.report(name, &message);
        Msg::Fail {
            id,
            name: name.into(),
            message,
        }
        .encode(out.tail());
    }

    fn dir(&mut self, name: String, meta: Meta, out: &mut OutQueue) -> Result<(), Halt> {
        if self.levels.len() >= MAX_DEPTH {
            return Err(Halt::Protocol(format!(
                "directories nested over {MAX_DEPTH} deep"
            )));
        }
        let (path, parent) = match self.place(&name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                self.levels.push(Level::Refused);
                return Ok(());
            }
            Err(m) => {
                self.fail(0, &name, m, out);
                self.levels.push(Level::Refused);
                return Ok(());
            }
        };
        let made = match std::fs::DirBuilder::new()
            .mode((meta.mode | 0o700) & 0o777)
            .create(&path)
        {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => false,
            Err(e) => {
                let m = match e.kind() {
                    std::io::ErrorKind::AlreadyExists => "exists and is not a directory".into(),
                    _ => reason(&e),
                };
                self.fail(0, &path.display().to_string(), m, out);
                self.levels.push(Level::Refused);
                return Ok(());
            }
        };
        if let Some(p) = parent {
            self.dirs[p].open += 1;
        }
        self.dirs.push(Dir {
            path,
            parent,
            meta,
            made,
            ended: false,
            open: 0,
        });
        self.levels.push(Level::Dir(self.dirs.len() - 1));
        Ok(())
    }

    /// Give directory `i` its final mode and times once it is ended and
    /// everything inside it is done, then try its parent.
    fn settle(&mut self, mut i: usize) {
        loop {
            let d = &self.dirs[i];
            if !d.ended || d.open > 0 {
                return;
            }
            let mode = match self.preserve {
                true => Some(d.meta.mode),
                false if d.made => Some(d.meta.mode & 0o777 & !self.umask),
                false => None,
            };
            let mut problem = None;
            if let Some(mode) = mode {
                let p = std::fs::Permissions::from_mode(mode);
                if let Err(e) = std::fs::set_permissions(&d.path, p) {
                    problem = Some(reason(&e));
                }
            }
            if self.preserve {
                if let Err(e) = File::open(&d.path).and_then(|f| set_times(&f, &d.meta)) {
                    problem = Some(reason(&e));
                }
            }
            if let Some(m) = problem {
                self.report(&d.path.display().to_string(), &m);
            }
            match d.parent {
                Some(p) => {
                    self.dirs[p].open -= 1;
                    i = p;
                }
                None => return,
            }
        }
    }

    fn file(
        &mut self,
        id: u32,
        name: String,
        size: u64,
        meta: Meta,
        out: &mut OutQueue,
    ) -> Result<(), Halt> {
        if id == 0 || self.parts.contains_key(&id) {
            return Err(Halt::Protocol(format!("FILE with id {id} out of place")));
        }
        let (dest, dir) = match self.place(&name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                self.fail(id, &name, "inside a refused directory".into(), out);
                return Ok(());
            }
            Err(m) => {
                self.fail(id, &name, m, out);
                return Ok(());
            }
        };
        let shown = dest.display().to_string();
        if dest.is_dir() {
            self.fail(id, &shown, "is a directory".into(), out);
            return Ok(());
        }
        let Some(part) = part_path(&dest) else {
            self.fail(id, &shown, "has no file name to write".into(), out);
            return Ok(());
        };
        let file = match open_part(&part, meta.mode & 0o777 & !self.umask) {
            Ok(f) => f,
            Err(e) => {
                self.fail(id, &shown, reason(&e), out);
                return Ok(());
            }
        };
        if let Some(d) = dir {
            self.dirs[d].open += 1;
        }
        self.parts.insert(
            id,
            Some(Part {
                file,
                part,
                dest,
                name: shown,
                size,
                written: 0,
                meta,
                dir,
            }),
        );
        Msg::Go { id, offset: 0 }.encode(out.tail());
        Ok(())
    }

    /// Part `id` is over, written or not: let its directory settle.
    fn close(&mut self, p: Part) {
        drop(p.file);
        if let Some(d) = p.dir {
            self.dirs[d].open -= 1;
            self.settle(d);
        }
    }

    /// Part `id` cannot be finished: remove it and say so.
    fn abandon(&mut self, id: u32, p: Part, message: String, out: &mut OutQueue) {
        let _ = std::fs::remove_file(&p.part);
        self.fail(id, &p.name.clone(), message, out);
        self.close(p);
    }

    fn data(&mut self, id: u32, bytes: &[u8], out: &mut OutQueue) -> Result<(), Halt> {
        let p = match self.parts.get_mut(&id) {
            Some(Some(p)) => p,
            Some(None) => return Ok(()),
            None => {
                return Err(Halt::Protocol(format!(
                    "DATA for file {id}, which is not open"
                )))
            }
        };
        if p.written + bytes.len() as u64 > p.size {
            return Err(Halt::Protocol(format!(
                "more DATA for {} than announced",
                p.name
            )));
        }
        match p.file.write_all(bytes) {
            Ok(()) => {
                p.written += bytes.len() as u64;
                self.outcome.bytes += bytes.len() as u64;
            }
            Err(e) => {
                let p = self.parts.remove(&id).flatten().expect("just found");
                self.abandon(id, p, reason(&e), out);
            }
        }
        Ok(())
    }

    fn file_end(&mut self, id: u32, out: &mut OutQueue) -> Result<(), Halt> {
        let p = match self.parts.remove(&id) {
            Some(Some(p)) => p,
            Some(None) => return Ok(()),
            None => {
                return Err(Halt::Protocol(format!(
                    "FILE_END for file {id}, which is not open"
                )))
            }
        };
        if p.written != p.size {
            let m = format!(
                "changed while being read: {} of {} bytes came",
                p.written, p.size
            );
            self.abandon(id, p, m, out);
            return Ok(());
        }
        let done = (|| {
            if self.preserve {
                sys::fchmod(p.file.as_raw_fd(), p.meta.mode)?;
            }
            std::fs::rename(&p.part, &p.dest)?;
            if self.preserve {
                set_times(&p.file, &p.meta)?;
            }
            Ok::<(), std::io::Error>(())
        })();
        match done {
            Ok(()) => {
                self.outcome.written += 1;
                Msg::Ok { id }.encode(out.tail());
                self.close(p);
            }
            Err(e) => self.abandon(id, p, reason(&e), out),
        }
        Ok(())
    }
}

impl Endpoint for Receiver {
    fn pump(&mut self, _out: &mut OutQueue) -> Result<(), Halt> {
        Ok(())
    }

    fn on_msg(&mut self, m: Msg<'_>, out: &mut OutQueue) -> Result<(), Halt> {
        match (self.state, m) {
            (_, Msg::Ping(n)) => Msg::Pong(n).encode(out.tail()),
            (_, Msg::Pong(_)) => {}
            (_, Msg::Error { message, .. }) => return Err(Halt::Ended(message)),
            (State::Hello, Msg::Hello(h)) => return self.hello(h, out),
            (State::Welcome, Msg::Welcome { .. }) => self.state = State::Running,
            (State::Running, Msg::Dir { name, meta }) => return self.dir(name, meta, out),
            (State::Running, Msg::DirEnd) => match self.levels.pop() {
                Some(Level::Dir(i)) => {
                    self.dirs[i].ended = true;
                    self.settle(i);
                }
                Some(Level::Refused) => {}
                None => return Err(Halt::Protocol("DIR_END outside any directory".into())),
            },
            (
                State::Running,
                Msg::File {
                    id,
                    name,
                    size,
                    meta,
                },
            ) => return self.file(id, name, size, meta, out),
            (State::Running, Msg::Data { id, bytes }) => return self.data(id, bytes, out),
            (State::Running, Msg::FileEnd { id }) => return self.file_end(id, out),
            (State::Running, Msg::Skip { name, reason }) => {
                self.outcome.skipped += 1;
                self.report(&name, &reason);
            }
            (State::Running, Msg::End) => {
                if !self.levels.is_empty() || self.parts.values().any(Option::is_some) {
                    return Err(Halt::Protocol("END with entries still open".into()));
                }
                Msg::Done {
                    written: self.outcome.written,
                    failed: self.outcome.failed,
                }
                .encode(out.tail());
                self.state = State::Done;
            }
            (_, m) => {
                return Err(Halt::Protocol(format!(
                    "unexpected message type {} from the sender",
                    m.ty()
                )))
            }
        }
        Ok(())
    }

    fn on_eof(&mut self) -> Result<(), Halt> {
        match self.state {
            State::Done => {
                self.state = State::Closed;
                Ok(())
            }
            _ => Err(Halt::Ended(
                "the sender went away before the copy finished".into(),
            )),
        }
    }

    fn finished(&self) -> bool {
        self.state == State::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Decoder;
    use crate::testutil::TempDir;

    fn opts() -> Options {
        let mut o = Options::new(true, false);
        o.report = false;
        o
    }

    /// A receiver driven message by message, its answers decoded.
    struct Rx {
        r: Receiver,
        out: OutQueue,
    }

    impl Rx {
        fn new(target: &Path, names: &[&str]) -> (Rx, Result<(), Halt>) {
            let mut rx = Rx {
                r: Receiver::serving(&opts()),
                out: OutQueue::default(),
            };
            let res = rx.send(Msg::Hello(Hello {
                version: crate::VERSION.into(),
                role: Role::Receive,
                recursive: true,
                preserve: false,
                verbose: false,
                target: target.to_string_lossy().into(),
                paths: names.iter().map(|s| s.to_string()).collect(),
            }));
            (rx, res)
        }

        fn send(&mut self, m: Msg<'_>) -> Result<(), Halt> {
            self.r.on_msg(m, &mut self.out)
        }

        /// The answers so far, as (type, a short description).
        fn answers(&mut self) -> Vec<String> {
            let mut d = Decoder::new();
            d.push(self.out.queued());
            self.out = OutQueue::default();
            let mut v = Vec::new();
            while let Some(m) = Msg::next(&mut d).unwrap() {
                v.push(match m {
                    Msg::Welcome { target_is_dir, .. } => format!("WELCOME dir={target_is_dir}"),
                    Msg::Go { id, offset } => format!("GO {id} {offset}"),
                    Msg::Ok { id } => format!("OK {id}"),
                    Msg::Fail { id, message, .. } => format!("FAIL {id} {message}"),
                    Msg::Error { message, .. } => format!("ERROR {message}"),
                    Msg::Done { written, failed } => format!("DONE {written} {failed}"),
                    other => format!("{other:?}"),
                });
            }
            v
        }

        fn file(&mut self, id: u32, name: &str, body: &[u8]) -> Result<(), Halt> {
            self.send(Msg::File {
                id,
                name: name.into(),
                size: body.len() as u64,
                meta: Meta {
                    mode: 0o640,
                    ..Meta::default()
                },
            })?;
            self.send(Msg::Data { id, bytes: body })?;
            self.send(Msg::FileEnd { id })
        }
    }

    fn dir_meta() -> Meta {
        Meta {
            mode: 0o755,
            ..Meta::default()
        }
    }

    /// A file refused is kept in the same map as the parts being written
    /// (acs-hofu.10): its late data is dropped, its id cannot be reused, an
    /// `END` with it never ended is not an entry left open, and once its
    /// `FILE_END` came the id is unknown.
    #[test]
    fn a_refused_file_is_dropped_not_open() {
        let t = TempDir::new();
        let (mut rx, r) = Rx::new(t.path(), &["a", "b"]);
        r.unwrap();
        let file = |id, name: &str| Msg::File {
            id,
            name: name.into(),
            size: 1,
            meta: Meta::default(),
        };
        // Not a name that was asked for: refused, with its data on the way.
        rx.send(file(1, "z")).unwrap();
        rx.send(Msg::Data { id: 1, bytes: b"Z" }).unwrap();
        assert!(matches!(rx.send(file(1, "a")), Err(Halt::Protocol(_))));
        rx.send(Msg::FileEnd { id: 1 }).unwrap();
        assert!(matches!(
            rx.send(Msg::Data { id: 1, bytes: b"Z" }),
            Err(Halt::Protocol(_))
        ));
        // Refused, and its FILE_END never sent: the copy still ends.
        rx.send(file(2, "y")).unwrap();
        rx.file(3, "a", b"A").unwrap();
        rx.send(Msg::End).unwrap();
        let answers = rx.answers();
        assert_eq!(
            answers.last().map(String::as_str),
            Some("DONE 1 2"),
            "{answers:?}"
        );
        assert!(!t.path().join("z").exists());
    }

    /// ACSCP_DESIGN §6's placement table, against a scratch directory.
    #[test]
    fn placement_follows_scps_rules() {
        let t = TempDir::new();
        let into = t.path().join("into");
        std::fs::create_dir(&into).unwrap();

        // One file into an existing directory: under its own name.
        let (mut rx, r) = Rx::new(&into, &["a"]);
        r.unwrap();
        rx.file(1, "a", b"A").unwrap();
        assert_eq!(std::fs::read(into.join("a")).unwrap(), b"A");

        // One file to anything else: that name, created or replaced.
        for dest in ["new", "new"] {
            let (mut rx, r) = Rx::new(&t.path().join(dest), &["a"]);
            r.unwrap();
            rx.file(1, "a", b"B").unwrap();
            assert_eq!(rx.answers(), ["WELCOME dir=false", "GO 1 0", "OK 1"]);
        }
        assert_eq!(std::fs::read(t.path().join("new")).unwrap(), b"B");

        // Several into an existing directory: each under its own name.
        let (mut rx, r) = Rx::new(&into, &["x", "y"]);
        r.unwrap();
        rx.file(1, "x", b"1").unwrap();
        rx.file(2, "y", b"2").unwrap();
        assert_eq!(std::fs::read(into.join("y")).unwrap(), b"2");

        // Several into what is not a directory: ERROR before any data.
        let (mut rx, r) = Rx::new(&t.path().join("nodir"), &["x", "y"]);
        assert!(matches!(r, Err(Halt::Ended(_))));
        assert_eq!(rx.answers().len(), 1);
        assert!(!t.path().join("nodir").exists());

        // One directory into an existing one: target/dirname/…
        let (mut rx, r) = Rx::new(&into, &["d"]);
        r.unwrap();
        rx.send(Msg::Dir {
            name: "d".into(),
            meta: dir_meta(),
        })
        .unwrap();
        rx.file(1, "f", b"in d").unwrap();
        rx.send(Msg::DirEnd).unwrap();
        assert_eq!(std::fs::read(into.join("d/f")).unwrap(), b"in d");

        // One directory to a missing path: created as that path.
        let missing = t.path().join("made");
        let (mut rx, r) = Rx::new(&missing, &["d"]);
        r.unwrap();
        rx.send(Msg::Dir {
            name: "d".into(),
            meta: dir_meta(),
        })
        .unwrap();
        rx.file(1, "f", b"x").unwrap();
        rx.send(Msg::DirEnd).unwrap();
        rx.send(Msg::End).unwrap();
        assert_eq!(std::fs::read(missing.join("f")).unwrap(), b"x");
        assert_eq!(rx.answers().last().unwrap(), "DONE 1 0");
    }

    /// CVE-2019-6111: a top-level name not asked for is refused, and
    /// nothing inside a refused directory is written.
    #[test]
    fn only_the_names_asked_for_are_accepted() {
        let t = TempDir::new();
        let (mut rx, r) = Rx::new(t.path(), &["notes.txt"]);
        r.unwrap();
        rx.file(1, ".bashrc", b"evil").unwrap();
        rx.send(Msg::Dir {
            name: ".ssh".into(),
            meta: dir_meta(),
        })
        .unwrap();
        rx.file(2, "authorized_keys", b"evil").unwrap();
        rx.send(Msg::DirEnd).unwrap();
        rx.file(3, "notes.txt", b"ok").unwrap();
        rx.send(Msg::End).unwrap();
        let a = rx.answers();
        assert!(a[1].starts_with("FAIL 1 refused"), "{a:?}");
        assert!(a[2].starts_with("FAIL 0 refused"), "{a:?}");
        assert!(a[3].starts_with("FAIL 2 inside a refused"), "{a:?}");
        assert_eq!(a[4..], ["GO 3 0", "OK 3", "DONE 1 3"]);
        let names: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["notes.txt"]);
    }

    #[test]
    fn nesting_and_ids_out_of_place_are_protocol_errors() {
        let t = TempDir::new();
        let (mut rx, _) = Rx::new(t.path(), &["a"]);
        assert!(matches!(rx.send(Msg::DirEnd), Err(Halt::Protocol(_))));

        let (mut rx, _) = Rx::new(t.path(), &["a"]);
        assert!(matches!(
            rx.send(Msg::Data { id: 9, bytes: b"x" }),
            Err(Halt::Protocol(_))
        ));

        let (mut rx, _) = Rx::new(t.path(), &["a"]);
        rx.send(Msg::File {
            id: 1,
            name: "a".into(),
            size: 1,
            meta: Meta::default(),
        })
        .unwrap();
        assert!(matches!(
            rx.send(Msg::Data {
                id: 1,
                bytes: b"xy"
            }),
            Err(Halt::Protocol(_))
        ));

        let (mut rx, _) = Rx::new(t.path(), &["a"]);
        for _ in 0..MAX_DEPTH {
            let name = if rx.r.levels.is_empty() { "a" } else { "d" };
            rx.send(Msg::Dir {
                name: name.into(),
                meta: dir_meta(),
            })
            .unwrap();
        }
        let deeper = rx.send(Msg::Dir {
            name: "d".into(),
            meta: dir_meta(),
        });
        assert!(matches!(deeper, Err(Halt::Protocol(_))));

        // A second HELLO, or a version that is not this one.
        let (mut rx, _) = Rx::new(t.path(), &["a"]);
        let again = rx.send(Msg::Hello(Hello {
            version: crate::VERSION.into(),
            role: Role::Receive,
            recursive: false,
            preserve: false,
            verbose: false,
            target: ".".into(),
            paths: vec!["a".into()],
        }));
        assert!(matches!(again, Err(Halt::Protocol(_))));
    }

    /// A download's receiver sent its `HELLO` to the remote sender: it
    /// takes a `WELCOME`, never a `HELLO`, and nothing before the
    /// `WELCOME`; then only the names it asked for land.
    #[test]
    fn a_download_receiver_waits_for_welcome_and_takes_only_its_names() {
        let t = TempDir::new();
        let asking = || Rx {
            r: Receiver::asking(t.path().into(), vec!["notes.txt".into()], &opts()),
            out: OutQueue::default(),
        };
        let mut rx = asking();
        let early = rx.file(1, "notes.txt", b"x");
        assert!(matches!(early, Err(Halt::Protocol(_))), "{early:?}");
        let mut rx = asking();
        let hello = rx.send(Msg::Hello(Hello {
            version: crate::VERSION.into(),
            role: Role::Receive,
            recursive: false,
            preserve: false,
            verbose: false,
            target: ".".into(),
            paths: vec!["notes.txt".into()],
        }));
        assert!(matches!(hello, Err(Halt::Protocol(_))), "{hello:?}");

        let mut rx = asking();
        rx.send(Msg::Welcome {
            umask: 0o022,
            target_is_dir: false,
        })
        .unwrap();
        rx.file(1, ".bashrc", b"evil").unwrap();
        rx.file(2, "notes.txt", b"notes").unwrap();
        rx.send(Msg::End).unwrap();
        let a = rx.answers();
        assert!(a[0].starts_with("FAIL 1 refused"), "{a:?}");
        assert_eq!(a[1..], ["GO 2 0", "OK 2", "DONE 1 1"], "{a:?}");
        assert!(!t.path().join(".bashrc").exists());
        assert_eq!(std::fs::read(t.path().join("notes.txt")).unwrap(), b"notes");
    }

    /// A file is written under `.<name>.acscp-part` and renamed only when
    /// whole; one that came short is removed and refused, and the final
    /// name never holds half a file.
    #[test]
    fn files_are_written_to_a_part_and_renamed_whole() {
        let t = TempDir::new();
        std::fs::write(t.path().join("f"), "old").unwrap();
        let (mut rx, _) = Rx::new(t.path(), &["f", "g"]);
        rx.send(Msg::File {
            id: 1,
            name: "f".into(),
            size: 4,
            meta: dir_meta(),
        })
        .unwrap();
        rx.send(Msg::Data {
            id: 1,
            bytes: b"ne",
        })
        .unwrap();
        assert_eq!(
            std::fs::read(t.path().join(".f.acscp-part")).unwrap(),
            b"ne"
        );
        assert_eq!(std::fs::read(t.path().join("f")).unwrap(), b"old");
        rx.send(Msg::Data {
            id: 1,
            bytes: b"w!",
        })
        .unwrap();
        rx.send(Msg::FileEnd { id: 1 }).unwrap();
        assert_eq!(std::fs::read(t.path().join("f")).unwrap(), b"new!");
        assert!(!t.path().join(".f.acscp-part").exists());

        // Short: the sender's file shrank.
        rx.send(Msg::File {
            id: 2,
            name: "g".into(),
            size: 10,
            meta: dir_meta(),
        })
        .unwrap();
        rx.send(Msg::Data {
            id: 2,
            bytes: b"abc",
        })
        .unwrap();
        rx.send(Msg::FileEnd { id: 2 }).unwrap();
        assert!(!t.path().join("g").exists());
        assert!(!t.path().join(".g.acscp-part").exists());
        let a = rx.answers();
        assert!(
            a.last()
                .unwrap()
                .starts_with("FAIL 2 changed while being read"),
            "{a:?}"
        );
    }

    /// A symlink planted at the part's name is not written through.
    #[test]
    fn a_planted_part_symlink_is_refused() {
        let t = TempDir::new();
        let victim = t.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let d = t.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::os::unix::fs::symlink(&victim, d.join(".f.acscp-part")).unwrap();
        let (mut rx, _) = Rx::new(&d, &["f"]);
        rx.file(1, "f", b"evil").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(!d.join("f").exists());
        assert!(rx.answers().last().unwrap().starts_with("FAIL 1"));
    }

    /// With headers running ahead, a directory's `DIR_END` can come before
    /// its files' data: its mode and times wait for them.
    #[test]
    fn a_directory_settles_after_its_contents() {
        let t = TempDir::new();
        let mut o = opts();
        o.preserve = true;
        let mut rx = Rx {
            r: Receiver::serving(&o),
            out: OutQueue::default(),
        };
        rx.send(Msg::Hello(Hello {
            version: crate::VERSION.into(),
            role: Role::Receive,
            recursive: true,
            preserve: true,
            verbose: false,
            target: t.path().to_string_lossy().into(),
            paths: vec!["d".into()],
        }))
        .unwrap();
        let when = Time {
            secs: 1_000_000_000,
            nanos: 500,
        };
        let meta = Meta {
            mode: 0o555,
            mtime: when,
            atime: when,
        };
        rx.send(Msg::Dir {
            name: "d".into(),
            meta,
        })
        .unwrap();
        rx.send(Msg::File {
            id: 1,
            name: "f".into(),
            size: 1,
            meta: Meta {
                mode: 0o600,
                mtime: when,
                atime: when,
            },
        })
        .unwrap();
        rx.send(Msg::DirEnd).unwrap();
        // Not settled yet: still writable, so the file can be renamed in.
        rx.send(Msg::Data { id: 1, bytes: b"x" }).unwrap();
        rx.send(Msg::FileEnd { id: 1 }).unwrap();
        let d = t.path().join("d");
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&d).unwrap();
        assert_eq!(m.mode() & 0o7777, 0o555);
        assert_eq!((m.mtime(), m.mtime_nsec()), (1_000_000_000, 500));
        let f = std::fs::metadata(d.join("f")).unwrap();
        assert_eq!(f.mode() & 0o7777, 0o600);
        assert_eq!(f.mtime(), 1_000_000_000);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn part_paths() {
        assert_eq!(
            part_path(Path::new("/a/b.txt")).unwrap(),
            Path::new("/a/.b.txt.acscp-part")
        );
        assert_eq!(
            part_path(Path::new("x")).unwrap(),
            Path::new(".x.acscp-part")
        );
        assert_eq!(part_path(Path::new("/")), None);
    }
}
