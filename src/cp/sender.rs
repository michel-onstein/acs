//! The sending endpoint (ACSCP_DESIGN §5.2, §5.3, §7): walk the sources
//! and stream them, running file headers ahead of data.
//!
//! A `FILE` needs the receiver's answer before its data, since only the
//! receiver knows what it holds of it already. Waiting for that per file
//! is a round trip each; instead the sender keeps up to a window of
//! headers out (`ACS_CP_WINDOW`, 64) while it streams the current file, so
//! by the time one file's last chunk goes the next one's `GO` is waiting
//! and only the first file pays the round trip. Directory entries need no
//! answer and never wait.
//!
//! The answer is `HAVE` where the receiver has a part of the file: the
//! sender hashes as much of its source, a slice per turn of the poll loop
//! so the pings go on, and sends from there if the two agree or `RESTART`s
//! the file if not. Over a new link ([`Endpoint::reconnect`]) the walk
//! starts again and leaves out every file already placed or refused.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use super::progress::Progress;
use super::proto::Msg;
use super::walk::{Entry, Walk};
use super::{note_on, Endpoint, Halt, Options, Outcome, Prefix, HIGH};
use crate::outq::OutQueue;

/// A file whose header is out and whose data is not all sent.
struct Slot {
    id: u32,
    path: PathBuf,
    file: File,
    size: u64,
    sent: u64,
    /// The receiver's `GO` came, or its `HAVE` was answered: data may flow.
    go: bool,
    /// A `HAVE` being checked: the source's prefix hashed so far, and the
    /// receiver's hash of its part.
    check: Option<(Prefix, [u8; 32])>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for `WELCOME`.
    Welcome,
    Streaming,
    /// `DONE` came.
    Done,
}

pub struct Sender {
    state: State,
    /// What the walk starts from, again over each new link.
    sources: Vec<(String, PathBuf)>,
    recursive: bool,
    walk: Walk,
    walked: bool,
    window: usize,
    /// The files out, oldest first. A `Vec`, not a `VecDeque`: taking the
    /// front moves a window of small slots once per file, and the binary
    /// keeps no second ring buffer for it (DESIGN §8.1).
    ahead: Vec<Slot>,
    /// Files whose `FILE` went on this link and whose `OK` or `FAIL` has
    /// not come yet.
    unanswered: HashMap<u32, PathBuf>,
    /// Files placed or refused: not sent again over a new link (§7).
    settled: HashSet<PathBuf>,
    /// What was reported already: a new link meets it again.
    told: HashSet<String>,
    next_id: u32,
    end_sent: bool,
    report: bool,
    verbose: bool,
    meter: Option<Progress>,
    outcome: Outcome,
}

impl Sender {
    /// A sender of `sources` (name, path). The `HELLO` is the caller's to
    /// send ([`super::hello`]); `welcomed` is a sender that answered one
    /// itself, as the remote end of a download does, and streams at once.
    pub fn new(sources: Vec<(String, PathBuf)>, opts: &Options, welcomed: bool) -> Sender {
        Sender {
            state: match welcomed {
                true => State::Streaming,
                false => State::Welcome,
            },
            walk: Walk::new(sources.clone(), opts.recursive),
            sources,
            recursive: opts.recursive,
            walked: false,
            window: opts.window.max(1),
            ahead: Vec::new(),
            unanswered: HashMap::new(),
            settled: HashSet::new(),
            told: HashSet::new(),
            next_id: 1,
            end_sent: false,
            report: opts.report,
            verbose: opts.report && opts.verbose,
            meter: match opts.meter {
                true => Progress::on_terminal(),
                false => None,
            },
            outcome: Outcome::default(),
        }
    }

    /// How the copy went, with what stopped it if anything did.
    pub fn outcome(mut self, error: Option<String>) -> Outcome {
        self.outcome.error = error;
        self.outcome
    }

    /// Report `what: why` the first time it happens; true if it was.
    fn tell(&mut self, what: &str, why: &str) -> bool {
        let first = self.told.insert(what.to_string());
        if first && self.report {
            note_on(what, why);
        }
        first
    }

    /// Queue the walk's next entry. False when the walk is over.
    fn next_entry(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(e) = self.walk.next() else {
            self.walked = true;
            return false;
        };
        match e {
            Entry::Dir { name, meta } => Msg::Dir { name, meta }.encode(out),
            Entry::DirEnd => Msg::DirEnd.encode(out),
            Entry::Skip { path, reason } => {
                if self.tell(&path, &reason) {
                    self.outcome.skipped += 1;
                    Msg::Skip { name: path, reason }.encode(out);
                }
            }
            Entry::File { path, .. } if self.settled.contains(&path) => {}
            Entry::File {
                name,
                path,
                file,
                size,
                meta,
            } => {
                let id = self.next_id;
                self.next_id += 1;
                Msg::File {
                    id,
                    name,
                    size,
                    meta,
                }
                .encode(out);
                self.unanswered.insert(id, path.clone());
                self.ahead.push(Slot {
                    id,
                    path,
                    file,
                    size,
                    sent: 0,
                    go: false,
                    check: None,
                });
            }
        }
        true
    }

    /// Queue the next chunk of the file at the front, or its end. False
    /// when it may not send yet.
    fn next_chunk(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(s) = self.ahead.first_mut() else {
            return false;
        };
        if !s.go {
            return false;
        }
        if s.sent < s.size {
            let file = &mut s.file;
            let left = s.size - s.sent;
            let n = super::proto::encode_data_from(
                s.id,
                left.min(usize::MAX as u64) as usize,
                out,
                |b| file.read(b),
            );
            if let Ok(n @ 1..) = n {
                s.sent += n as u64;
                self.outcome.bytes += n as u64;
                if let Some(m) = &mut self.meter {
                    m.add(n as u64);
                }
                return true;
            }
            // Short: the file shrank, or could not be read. The end goes
            // now and the receiver, seeing fewer bytes than announced,
            // refuses the file rather than keep a wrong copy (§6).
        }
        Msg::FileEnd { id: s.id }.encode(out);
        self.ahead.remove(0);
        if let Some(m) = &mut self.meter {
            m.finish();
        }
        self.start_meter();
        true
    }

    /// Point the meter at the file now at the front, counting what the
    /// receiver had of it already.
    fn start_meter(&mut self) {
        if let (Some(m), Some(s)) = (&mut self.meter, self.ahead.first()) {
            m.start(&s.path.display().to_string(), s.size);
            if s.sent > 0 {
                m.add(s.sent);
            }
        }
    }

    /// Hash the next slice of a source a `HAVE` is being checked against,
    /// and once all of it is in, send from the end of the receiver's part
    /// or `RESTART` the file (ACSCP_DESIGN §7).
    fn check(&mut self, out: &mut Vec<u8>) -> Result<(), Halt> {
        let Some(at) = self.ahead.iter().position(|s| s.check.is_some()) else {
            return Ok(());
        };
        let s = &mut self.ahead[at];
        let (p, want) = s.check.as_mut().expect("found");
        let same = match p.step(&mut s.file) {
            Ok(None) => return Ok(()),
            Ok(Some(h)) => h == *want,
            // Shorter than the part, or unreadable: not its start.
            Err(_) => false,
        };
        let len = p.len;
        s.check = None;
        s.go = true;
        let shown = s.path.display().to_string();
        if same {
            s.sent = len;
        } else {
            s.file
                .seek(SeekFrom::Start(0))
                .map_err(|e| Halt::Ended(format!("{shown}: {e}")))?;
            Msg::Restart { id: s.id }.encode(out);
        }
        if same && at == 0 {
            if let Some(m) = &mut self.meter {
                m.add(len);
            }
        }
        if self.verbose {
            match same {
                true => note_on(&shown, &format!("resuming after {len} bytes already there")),
                false => note_on(&shown, "the part there is not its start — sending it whole"),
            }
        }
        Ok(())
    }
}

impl Endpoint for Sender {
    fn pump(&mut self, out: &mut OutQueue) -> Result<(), Halt> {
        if self.state != State::Streaming {
            return Ok(());
        }
        self.check(out.tail())?;
        while out.len() < HIGH {
            let had = self.ahead.len();
            if self.ahead.len() < self.window && !self.walked && self.next_entry(out.tail()) {
                if had == 0 && self.ahead.len() == 1 {
                    self.start_meter();
                }
                continue;
            }
            if self.next_chunk(out.tail()) {
                continue;
            }
            if self.walked && self.ahead.is_empty() && !self.end_sent {
                Msg::End.encode(out.tail());
                self.end_sent = true;
            }
            break;
        }
        Ok(())
    }

    fn on_msg(&mut self, m: Msg<'_>, _out: &mut OutQueue) -> Result<(), Halt> {
        match m {
            Msg::Welcome { .. } if self.state == State::Welcome => self.state = State::Streaming,
            Msg::Error { message, .. } => return Err(Halt::Ended(message)),
            Msg::Go { id, offset } if self.state == State::Streaming => {
                let s = self.waiting(id)?;
                if offset > s.size {
                    return Err(Halt::Protocol(format!("GO for file {id} out of place")));
                }
                if offset > 0 {
                    if let Err(e) = s.file.seek(SeekFrom::Start(offset)) {
                        return Err(Halt::Ended(format!("{}: {e}", s.path.display())));
                    }
                }
                s.sent = offset;
                s.go = true;
            }
            Msg::Have { id, len, hash } if self.state == State::Streaming => {
                let s = self.waiting(id)?;
                // Longer than the file: the check could only fail.
                let len = len.min(s.size.saturating_add(1));
                s.check = Some((Prefix::new(len), hash));
            }
            Msg::Ok { id } if self.state == State::Streaming => {
                if let Some(p) = self.unanswered.remove(&id) {
                    self.settled.insert(p);
                }
                self.outcome.written += 1;
            }
            Msg::Fail { id, name, message } if self.state == State::Streaming => {
                let path = match id {
                    0 => None,
                    _ => self.unanswered.remove(&id),
                };
                if let Some(at) = self.ahead.iter().position(|s| s.id == id && id != 0) {
                    self.ahead.remove(at);
                    if at == 0 {
                        self.start_meter();
                    }
                }
                let what = match path {
                    Some(p) => {
                        let shown = p.display().to_string();
                        self.settled.insert(p);
                        shown
                    }
                    None => name,
                };
                if self.tell(&what, &message) {
                    self.outcome.failed += 1;
                }
            }
            Msg::Done { .. } if self.end_sent => self.state = State::Done,
            m => {
                return Err(Halt::Protocol(format!(
                    "unexpected message type {} from the receiver",
                    m.ty()
                )))
            }
        }
        Ok(())
    }

    fn on_eof(&mut self) -> Result<(), Halt> {
        match self.state {
            State::Done => Ok(()),
            _ => Err(Halt::Lost(
                "the receiver went away before the copy finished".into(),
            )),
        }
    }

    fn finished(&self) -> bool {
        self.state == State::Done
    }

    fn welcomed(&self) -> bool {
        self.state != State::Welcome
    }

    fn busy(&self) -> bool {
        self.ahead.iter().any(|s| s.check.is_some())
    }

    /// Start the walk again for a new receiver, leaving out what is placed.
    fn reconnect(&mut self) {
        self.state = State::Welcome;
        self.walk = Walk::new(self.sources.clone(), self.recursive);
        self.walked = false;
        self.ahead.clear();
        self.unanswered.clear();
        self.end_sent = false;
    }
}

impl Sender {
    /// The slot of file `id`, still waiting for its answer.
    fn waiting(&mut self, id: u32) -> Result<&mut Slot, Halt> {
        match self.ahead.iter_mut().find(|s| s.id == id) {
            Some(s) if !s.go && s.check.is_none() => Ok(s),
            _ => Err(Halt::Protocol(format!(
                "an answer for file {id}, which is not waiting"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Decoder;
    use crate::testutil::TempDir;

    /// What `s` queued, as short descriptions.
    fn sent(s: &mut Sender, out: &mut OutQueue) -> Vec<String> {
        s.pump(out).unwrap();
        let mut d = Decoder::new();
        d.push(out.queued());
        *out = OutQueue::default();
        let mut v = Vec::new();
        while let Some(m) = Msg::next(&mut d).unwrap() {
            v.push(match m {
                Msg::File { id, name, .. } => format!("FILE {id} {name}"),
                Msg::Data { id, bytes } => format!("DATA {id} {}", String::from_utf8_lossy(bytes)),
                Msg::FileEnd { id } => format!("FILE_END {id}"),
                Msg::Restart { id } => format!("RESTART {id}"),
                Msg::End => "END".into(),
                other => format!("{other:?}"),
            });
        }
        v
    }

    fn welcome() -> Msg<'static> {
        Msg::Welcome {
            umask: 0o022,
            target_is_dir: true,
        }
    }

    /// `HAVE` is answered once the source's own start is hashed: data from
    /// the end of the part when they agree, `RESTART` and all of it when
    /// not (ACSCP_DESIGN §7).
    #[test]
    fn a_have_is_checked_against_the_source() {
        let t = TempDir::new();
        std::fs::write(t.path().join("a"), b"abcdef").unwrap();
        std::fs::write(t.path().join("b"), b"uvwxyz").unwrap();
        let mut opts = Options::new(false, false);
        opts.report = false;
        let srcs = ["a", "b"].map(|n| (n.to_string(), t.path().join(n)));
        let mut s = Sender::new(srcs.to_vec(), &opts, false);
        let mut out = OutQueue::default();
        s.on_msg(welcome(), &mut out).unwrap();
        assert_eq!(sent(&mut s, &mut out), ["FILE 1 a", "FILE 2 b"]);
        let hash = |b: &[u8]| crate::sha256::digest(b);
        let have = |id, b: &[u8]| Msg::Have {
            id,
            len: b.len() as u64,
            hash: hash(b),
        };
        s.on_msg(have(1, b"abc"), &mut out).unwrap();
        s.on_msg(have(2, b"uvX"), &mut out).unwrap();
        assert!(s.busy());
        let mut all = Vec::new();
        while s.busy() || all.last().map(String::as_str) != Some("END") {
            all.extend(sent(&mut s, &mut out));
        }
        assert_eq!(
            all,
            [
                "DATA 1 def",
                "FILE_END 1",
                "RESTART 2",
                "DATA 2 uvwxyz",
                "FILE_END 2",
                "END"
            ]
        );
    }

    /// Over a new link the walk starts again and leaves out what the old
    /// one placed or refused; what was in flight is sent again, and what
    /// was skipped is not reported twice (ACSCP_DESIGN §7).
    #[test]
    fn a_new_link_leaves_out_what_was_placed() {
        let t = TempDir::new();
        for n in ["a", "b", "c"] {
            std::fs::write(t.path().join(n), n).unwrap();
        }
        let mut opts = Options::new(false, false);
        opts.report = false;
        let srcs = ["a", "b", "c", "missing"].map(|n| (n.to_string(), t.path().join(n)));
        let mut s = Sender::new(srcs.to_vec(), &opts, false);
        let mut out = OutQueue::default();
        s.on_msg(welcome(), &mut out).unwrap();
        let first = sent(&mut s, &mut out);
        assert_eq!(
            &first[..3],
            ["FILE 1 a", "FILE 2 b", "FILE 3 c"],
            "{first:?}"
        );
        assert!(first[3].starts_with("Skip"), "{first:?}");
        s.on_msg(Msg::Go { id: 1, offset: 0 }, &mut out).unwrap();
        s.on_msg(Msg::Go { id: 2, offset: 0 }, &mut out).unwrap();
        sent(&mut s, &mut out);
        s.on_msg(Msg::Ok { id: 1 }, &mut out).unwrap();
        let fail = Msg::Fail {
            id: 2,
            name: "b".into(),
            message: "no room".into(),
        };
        s.on_msg(fail, &mut out).unwrap();
        // The link goes with c's header out and unanswered.
        s.reconnect();
        assert!(!s.welcomed());
        assert!(sent(&mut s, &mut out).is_empty(), "sent before WELCOME");
        s.on_msg(welcome(), &mut out).unwrap();
        assert_eq!(sent(&mut s, &mut out), ["FILE 4 c"]);
        let o = s.outcome(None);
        assert_eq!((o.written, o.failed, o.skipped), (1, 1, 1));
    }
}
