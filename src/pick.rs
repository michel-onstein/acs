//! What a plain `acs [user@]<host>` attaches to (DESIGN §4.4): the host's
//! sessions are listed first; with none detached a session is created,
//! otherwise the user picks one from the menu (`menu.rs`), ends some, or
//! leaves. All of it happens on the session's own connection
//! (`_proxy --pick`), which the attach then goes on over.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use crate::cli::{ClientArgs, Target};
use crate::client::{self, code, Link, Picked};
use crate::list::Failure;
use crate::menu::{Choice, Menu};
use crate::proto::{self, Decoder, Marker, Msg, StatusInfo};
use crate::ssh::{self, Call};
use crate::sys;
use crate::tty::{self, AltScreen, RawMode};

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;

/// Settle a [`Target::Pick`]: to the session the user picked (with `force`
/// set for a takeover they confirmed), or to a new session. `Ok(Some)` is
/// the connection to attach over — the one the list came on, so the list
/// and the attach reach the same machine with one ssh call; `Ok(None)`
/// leaves the attach to dial its own (no terminal for a menu, acs not yet
/// installed there, or the connection lost while the menu was up). `Err` is
/// the exit status when the user left the menu or the host could not be
/// asked.
pub fn choose(args: &mut ClientArgs) -> Result<Option<Picked>, u8> {
    let Target::Pick(default) = args.target.clone() else {
        return Ok(None);
    };
    // No terminal for a menu: the default session, as ever.
    if !(sys::isatty(STDIN) && sys::isatty(STDOUT)) {
        args.target = Target::Named(default);
        return Ok(None);
    }
    let host = args.host_name().to_string();
    let fail = |f: Failure| {
        eprintln!("acs: {}", f.message(&host));
        f.code()
    };
    let timeout = client::answer_timeout(false);
    let remote = ssh::remote_acs(crate::VERSION, &["_proxy", "--pick"]);
    let (link, marker) = client::dial(args, Call::Session, &remote, timeout)
        .map_err(|e| fail(Failure::Unreachable(e.to_string())))?;
    let mut pick = match marker {
        Marker::Ready { proto: p, rest } if p == proto::PROTO_VERSION => {
            let mut dec = Decoder::new();
            dec.push(&rest);
            Pick {
                link,
                dec,
                timeout,
                lost: false,
            }
        }
        Marker::Ready { proto: p, .. } => {
            link.close();
            eprintln!(
                "acs: remote acs speaks protocol {p}, this client {}",
                proto::PROTO_VERSION
            );
            return Err(code::ERROR);
        }
        // acs is not installed there yet: the attach installs it, and there
        // is nothing to pick.
        Marker::Need { .. } => {
            link.close();
            args.target = new_session(&[], &default);
            return Ok(None);
        }
    };
    let sessions = match pick.list() {
        Ok(a) => a.sessions,
        Err(f) => {
            pick.link.close();
            return Err(fail(f));
        }
    };
    if !sessions.iter().any(|s| !s.attached) {
        args.target = new_session(&sessions, &default);
        return Ok(Some(pick.into_picked()));
    }
    let mut menu = Menu::new(sessions, args.force);
    let choice = run_menu(&mut pick, &host, &mut menu);
    match choice {
        Ok(Choice::Attach { name, force }) => {
            args.target = Target::Named(name);
            args.force |= force;
        }
        Ok(Choice::New) => args.target = new_session(menu.sessions(), &default),
        Ok(Choice::Leave(c)) => {
            pick.link.close();
            return Err(c);
        }
        // Ended in the menu, which goes on.
        Ok(Choice::Kill(_)) => unreachable!(),
        Err(e) => {
            pick.link.close();
            eprintln!("acs: {e}");
            return Err(code::ERROR);
        }
    }
    if pick.lost {
        pick.link.close();
        return Ok(None);
    }
    Ok(Some(pick.into_picked()))
}

/// The pick's connection: `_proxy --pick` past its marker.
struct Pick {
    link: Link,
    dec: Decoder,
    /// How long the host has for each answer (DESIGN §5.3).
    timeout: Duration,
    /// The connection failed while the menu was up: the attach dials anew.
    lost: bool,
}

/// One list from the proxy: every session's STATUS, and the message of an
/// ERROR sent along (a session that would not end).
struct Answer {
    sessions: Vec<StatusInfo>,
    error: Option<String>,
}

impl Pick {
    fn into_picked(mut self) -> Picked {
        Picked {
            rest: self.dec.take_rest(),
            link: self.link,
        }
    }

    /// Read one list, up to its LIST_END, within the timeout.
    fn list(&mut self) -> Result<Answer, Failure> {
        let deadline = Instant::now() + self.timeout;
        let from = self.link.from_fd().as_raw_fd();
        let mut answer = Answer {
            sessions: Vec::new(),
            error: None,
        };
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.dec.next_msg() {
                Ok(Some(Msg::StatusReply(s))) => answer.sessions.push(s),
                Ok(Some(Msg::Error { message, .. })) => answer.error = Some(message),
                Ok(Some(Msg::ListEnd)) => return Ok(answer),
                Ok(Some(other)) => {
                    return Err(Failure::BadReply(format!("unexpected {other:?}")));
                }
                Ok(None) => match wait_readable(from, deadline) {
                    Ok(true) => match sys::read(from, &mut buf) {
                        Ok(0) => {
                            return Err(Failure::Unreachable(
                                "the connection closed before the session list".into(),
                            ))
                        }
                        Ok(n) => self.dec.push(&buf[..n]),
                        Err(e) => return Err(Failure::Unreachable(e.to_string())),
                    },
                    Ok(false) => {
                        return Err(Failure::Unreachable(format!(
                            "no answer within {} s",
                            self.timeout.as_secs_f32()
                        )))
                    }
                    Err(e) => return Err(Failure::Unreachable(e.to_string())),
                },
                Err(e) => return Err(Failure::BadReply(e.to_string())),
            }
        }
    }

    /// End `name` on the host and read the list that answers it.
    fn end(&mut self, name: &str) -> Result<Answer, Failure> {
        let frame = Msg::EndSession { name: name.into() }.to_bytes();
        sys::write_all(self.link.to_fd().as_raw_fd(), &frame)
            .map_err(|e| Failure::Unreachable(e.to_string()))?;
        self.list()
    }
}

/// Wait until `fd` is readable or `deadline` passes (`false`).
fn wait_readable(fd: RawFd, deadline: Instant) -> io::Result<bool> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut p = [sys::pollfd(fd, libc::POLLIN)];
        match sys::poll(&mut p, left.as_millis().min(i32::MAX as u128) as i32) {
            Ok(0) => continue, // the deadline, or a signal cut the wait short
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// A new session: named `default` if no session has that name, otherwise
/// the lowest free number (as `--new`).
fn new_session(sessions: &[StatusInfo], default: &str) -> Target {
    if sessions.iter().any(|s| s.name == default) {
        Target::New
    } else {
        Target::Named(default.to_string())
    }
}

/// Show the menu until the user picks a session, a new one, or leaving.
/// Ending a session happens here, and the menu goes on with what is left.
/// The terminal comes back as it was on every way out: the guards on
/// return, the emergency restore on a signal or a panic.
fn run_menu(pick: &mut Pick, host: &str, menu: &mut Menu) -> io::Result<Choice> {
    tty::install_emergency_restore()?;
    let signals = sys::signals::install(&[libc::SIGWINCH])?;
    let _ = sys::signals::ignore(libc::SIGPIPE);
    let _raw = RawMode::enter(STDIN)?;
    let _screen = AltScreen::enter(STDOUT)?;
    let mut buf = [0u8; 1024];
    loop {
        draw(menu, host)?;
        let timeout = match menu.deadline() {
            Some(d) => d.saturating_sub(sys::now_ms()).min(i32::MAX as u64) as i32,
            None => -1,
        };
        let mut fds = [
            sys::pollfd(STDIN, libc::POLLIN),
            sys::pollfd(signals.as_raw_fd(), libc::POLLIN),
        ];
        sys::poll(&mut fds, timeout)?;
        if fds[1].revents != 0 {
            // A resize: the next draw fits the new size.
            sys::signals::drain(signals.as_raw_fd());
        }
        let choice = if fds[0].revents != 0 {
            match sys::read(STDIN, &mut buf)? {
                // The terminal went away.
                0 => return Ok(Choice::Leave(code::ERROR)),
                n => menu.feed(&buf[..n], sys::now_ms()),
            }
        } else {
            menu.tick(sys::now_ms())
        };
        match choice {
            None => {}
            Some(Choice::Kill(name)) => end(pick, host, menu, &name),
            Some(c) => return Ok(c),
        }
    }
}

fn draw(menu: &Menu, host: &str) -> io::Result<()> {
    let size = sys::get_winsize(STDIN).unwrap_or_default();
    let (cols, rows) = match (size.cols, size.rows) {
        (0, _) | (_, 0) => (80, 24),
        (c, r) => (c as usize, r as usize),
    };
    sys::write_all(
        STDOUT,
        menu.render(host, sys::unix_now(), cols, rows).as_bytes(),
    )
}

/// End `name` on the host, over the pick's connection (END_SESSION, which
/// the proxy answers with the sessions left), and show the menu with those.
/// A failed connection is noted; the attach then dials its own.
fn end(pick: &mut Pick, host: &str, menu: &mut Menu, name: &str) {
    if pick.lost {
        menu.set_note(format!("the connection to {host} was lost"));
        return;
    }
    menu.set_note(format!("ending session '{name}'…"));
    let _ = draw(menu, host);
    let note = match pick.end(name) {
        Ok(a) => {
            let gone = !a.sessions.iter().any(|s| s.name == name);
            menu.set_sessions(a.sessions);
            match (a.error, gone) {
                (Some(e), _) => e,
                (None, true) => format!("session '{name}' ended"),
                (None, false) => format!("session '{name}' is still there"),
            }
        }
        Err(f) => {
            pick.lost = true;
            f.message(host)
        }
    };
    menu.set_note(note);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> StatusInfo {
        StatusInfo {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_new_session_is_the_default_name_until_that_is_taken() {
        assert_eq!(new_session(&[], "main"), Target::Named("main".into()));
        assert_eq!(
            new_session(&[info("1"), info("work")], "main"),
            Target::Named("main".into())
        );
        assert_eq!(new_session(&[info("main")], "main"), Target::New);
        assert_eq!(
            new_session(&[info("main")], "michel"),
            Target::Named("michel".into())
        );
    }
}
