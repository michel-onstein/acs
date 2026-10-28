//! The sending endpoint (ACSCP_DESIGN §5.2, §5.3): walk the sources and
//! stream them, running file headers ahead of data.
//!
//! A `FILE` needs the receiver's `GO` before its data, since only the
//! receiver knows what it holds of it already. Waiting for that per file
//! is a round trip each; instead the sender keeps up to a window of
//! headers out (`ACS_CP_WINDOW`, 64) while it streams the current file, so
//! by the time one file's last chunk goes the next one's `GO` is waiting
//! and only the first file pays the round trip. Directory entries need no
//! answer and never wait.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use super::progress::Progress;
use super::proto::{Hello, Msg, Role};
use super::walk::{Entry, Walk};
use super::{note_on, Endpoint, Halt, Options, Outcome, HIGH};
use crate::outq::OutQueue;

/// A file whose header is out and whose data is not all sent.
struct Slot {
    id: u32,
    path: PathBuf,
    file: File,
    size: u64,
    sent: u64,
    /// The receiver's `GO` came: data may flow.
    go: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The `HELLO` is still to be queued.
    Hello,
    /// Waiting for `WELCOME`.
    Welcome,
    Streaming,
    /// `DONE` came.
    Done,
}

pub struct Sender {
    state: State,
    hello: Option<Hello>,
    walk: Walk,
    walked: bool,
    window: usize,
    /// The files out, oldest first. A `Vec`, not a `VecDeque`: taking the
    /// front moves a window of small slots once per file, and the binary
    /// keeps no second ring buffer for it (DESIGN §8.1).
    ahead: Vec<Slot>,
    next_id: u32,
    end_sent: bool,
    report: bool,
    meter: Option<Progress>,
    outcome: Outcome,
}

impl Sender {
    /// A sender of `sources` (name, path) that asks its receiver to write
    /// them to `target`.
    pub fn new(target: &str, sources: Vec<(String, PathBuf)>, opts: &Options) -> Sender {
        let hello = Hello {
            version: crate::VERSION.into(),
            role: Role::Receive,
            recursive: opts.recursive,
            preserve: opts.preserve,
            verbose: opts.verbose,
            target: target.into(),
            paths: sources.iter().map(|(n, _)| n.clone()).collect(),
        };
        Sender {
            state: State::Hello,
            hello: Some(hello),
            walk: Walk::new(sources, opts.recursive),
            walked: false,
            window: opts.window.max(1),
            ahead: Vec::new(),
            next_id: 1,
            end_sent: false,
            report: opts.report,
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

    fn report(&self, what: &str, why: &str) {
        if self.report {
            note_on(what, why);
        }
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
                self.report(&path, &reason);
                self.outcome.skipped += 1;
                Msg::Skip { name: path, reason }.encode(out);
            }
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
                self.ahead.push(Slot {
                    id,
                    path,
                    file,
                    size,
                    sent: 0,
                    go: false,
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

    /// Point the meter at the file now at the front.
    fn start_meter(&mut self) {
        if let (Some(m), Some(s)) = (&mut self.meter, self.ahead.first()) {
            m.start(&s.path.display().to_string(), s.size);
        }
    }
}

impl Endpoint for Sender {
    fn pump(&mut self, out: &mut OutQueue) -> Result<(), Halt> {
        match self.state {
            State::Hello => {
                if let Some(h) = self.hello.take() {
                    Msg::Hello(h).encode(out.tail());
                }
                self.state = State::Welcome;
                return Ok(());
            }
            State::Welcome | State::Done => return Ok(()),
            State::Streaming => {}
        }
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

    fn on_msg(&mut self, m: Msg<'_>, out: &mut OutQueue) -> Result<(), Halt> {
        match m {
            Msg::Welcome { .. } if self.state == State::Welcome => self.state = State::Streaming,
            Msg::Error { message, .. } => return Err(Halt::Ended(message)),
            Msg::Go { id, offset } if self.state == State::Streaming => {
                let Some(s) = self.ahead.iter_mut().find(|s| s.id == id) else {
                    return Err(Halt::Protocol(format!(
                        "GO for file {id}, which is not waiting"
                    )));
                };
                if s.go || offset > s.size {
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
            Msg::Ok { .. } if self.state == State::Streaming => self.outcome.written += 1,
            Msg::Fail { id, name, message } if self.state == State::Streaming => {
                self.outcome.failed += 1;
                let at = self.ahead.iter().position(|s| s.id == id && id != 0);
                let what = match at.map(|i| self.ahead.remove(i)) {
                    Some(s) => {
                        if at == Some(0) {
                            self.start_meter();
                        }
                        s.path.display().to_string()
                    }
                    None => name,
                };
                self.report(&what, &message);
            }
            Msg::Done { .. } if self.end_sent => self.state = State::Done,
            Msg::Ping(n) => Msg::Pong(n).encode(out.tail()),
            Msg::Pong(_) => {}
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
            _ => Err(Halt::Ended(
                "the receiver went away before the copy finished".into(),
            )),
        }
    }

    fn finished(&self) -> bool {
        self.state == State::Done
    }
}
