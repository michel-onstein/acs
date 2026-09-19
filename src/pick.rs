//! What a plain `acs [user@]<host>` attaches to (DESIGN §4.4): the host's
//! sessions are listed first; with none detached a session is created,
//! otherwise the user picks one from the menu (`menu.rs`), ends some, or
//! leaves. All of it happens on the session's own connection
//! (`_proxy --pick`), which the attach then goes on over. `acs list <host>`
//! in a terminal shows the same menu whatever is detached.
//!
//! `acs list` in a terminal shows the menu over every host alias at once
//! (§7.3): each host asked in parallel as `acs list` asks it, its rows in as
//! it answers, a session ended over a short `_proxy --pick` call of its
//! own, and the one picked attached as `acs <alias> <session>` would.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::cli::{ClientArgs, Target};
use crate::client::{self, code, Link, Picked};
use crate::list::{self, Failure};
use crate::menu::{Answer, Choice, Menu};
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
/// asked. `always` shows the menu even with nothing detached
/// (`acs list <host>`).
pub fn choose(args: &mut ClientArgs, always: bool) -> Result<Option<Picked>, u8> {
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
    let (mut pick, sessions) =
        match open_pick(args, Call::Session, client::answer_timeout(false)).map_err(fail)? {
            Some(p) => p,
            // acs is not installed there yet: the attach installs it, and
            // there is nothing to pick.
            None => {
                args.target = new_session(&[], &default);
                return Ok(None);
            }
        };
    if !always && !sessions.iter().any(|s| !s.attached) {
        args.target = new_session(&sessions, &default);
        return Ok(Some(pick.into_picked()));
    }
    let mut menu = Menu::new(sessions, args.force);
    let choice = run_menu(&mut menu, &host, None, &mut |menu, _, name| {
        end(&mut pick, &host, menu, name)
    });
    match choice {
        Ok(Choice::Attach { name, force, .. }) => {
            args.target = Target::Named(name);
            args.force |= force;
        }
        Ok(Choice::New { .. }) => args.target = new_session(menu.sessions(0), &default),
        Ok(Choice::Leave(c)) => {
            pick.link.close();
            return Err(c);
        }
        // Ended in the menu, which goes on.
        Ok(Choice::Kill { .. }) => unreachable!(),
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

/// Dial `_proxy --pick` and read its first list: `Ok(None)` if acs of our
/// version is not installed there.
fn open_pick(
    args: &ClientArgs,
    call: Call,
    timeout: Duration,
) -> Result<Option<(Pick, Vec<StatusInfo>)>, Failure> {
    let remote = ssh::remote_acs(crate::VERSION, &["_proxy", "--pick"]);
    let (link, marker) = client::dial(args, call, &remote, timeout)
        .map_err(|e| Failure::Unreachable(e.to_string()))?;
    let rest = match marker {
        Marker::Ready { proto: p, rest } if p == proto::PROTO_VERSION => rest,
        Marker::Ready { proto: p, .. } => {
            link.close();
            return Err(Failure::BadReply(format!(
                "remote acs speaks protocol {p}, this client {}",
                proto::PROTO_VERSION
            )));
        }
        Marker::Need { .. } => {
            link.close();
            return Ok(None);
        }
    };
    let mut dec = Decoder::new();
    dec.push(&rest);
    let mut pick = Pick {
        link,
        dec,
        timeout,
        lost: false,
    };
    match pick.list() {
        Ok(a) => Ok(Some((pick, a.sessions))),
        Err(f) => {
            pick.link.close();
            Err(f)
        }
    }
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
struct Listing {
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
    fn list(&mut self) -> Result<Listing, Failure> {
        let deadline = Instant::now() + self.timeout;
        let from = self.link.from_fd().as_raw_fd();
        let mut answer = Listing {
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
    fn end(&mut self, name: &str) -> Result<Listing, Failure> {
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

/// What ending a session does: `(menu, host, name)`.
type EndSession<'a> = dyn FnMut(&mut Menu, usize, &str) + 'a;

/// A second thing to wait for besides the keys: `fd` turns readable, and
/// `ready` brings what came into the menu.
struct Wake<'a> {
    fd: RawFd,
    ready: &'a mut dyn FnMut(&mut Menu),
}

/// Show the menu until the user picks a session, a new one, or leaving.
/// Ending a session happens here (`end`), and the menu goes on with what is
/// left. The terminal comes back as it was on every way out: the guards on
/// return, the emergency restore on a signal or a panic.
fn run_menu(
    menu: &mut Menu,
    host: &str,
    mut wake: Option<Wake>,
    end: &mut EndSession,
) -> io::Result<Choice> {
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
            sys::pollfd(wake.as_ref().map_or(-1, |w| w.fd), libc::POLLIN),
        ];
        sys::poll(&mut fds, timeout)?;
        if fds[1].revents != 0 {
            // A resize: the next draw fits the new size.
            sys::signals::drain(signals.as_raw_fd());
        }
        if fds[2].revents != 0 {
            if let Some(w) = wake.as_mut() {
                (w.ready)(menu);
            }
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
            Some(Choice::Kill { host: h, name }) => end(menu, h, &name),
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
        Ok(a) => ended(menu, 0, name, a),
        Err(f) => {
            pick.lost = true;
            f.message(host)
        }
    };
    menu.set_note(note);
}

/// Show host `h`'s sessions left after ending `name`; what to say about it.
fn ended(menu: &mut Menu, h: usize, name: &str, a: Listing) -> String {
    let gone = !a.sessions.iter().any(|s| s.name == name);
    menu.set_sessions(h, a.sessions);
    match (a.error, gone) {
        (Some(e), _) => e,
        (None, true) => format!("session '{name}' ended"),
        (None, false) => format!("session '{name}' is still there"),
    }
}

// ---- every host --------------------------------------------------------------

/// `acs list` in a terminal: the session menu over every host alias (DESIGN
/// §7.3). Returns the exit status: the attached session's, or the menu's.
pub fn every_host(args: &ClientArgs) -> u8 {
    let names: Vec<String> = args.config.hosts.iter().map(|a| a.name.clone()).collect();
    if names.is_empty() {
        eprintln!("acs: {}", list::NO_ALIASES);
        return code::USAGE;
    }
    let (wake_r, wake_w) = match sys::pipe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("acs: {e}");
            return code::ERROR;
        }
    };
    let _ = sys::set_nonblocking(wake_r.as_raw_fd(), true);
    let answers = ask_every_host(args, &names, Arc::new(wake_w));
    let mut menu = Menu::every_host(names.clone(), args.force);
    let mut ready = |menu: &mut Menu| {
        let mut buf = [0u8; 64];
        while matches!(sys::read(wake_r.as_raw_fd(), &mut buf), Ok(n) if n > 0) {}
        while let Ok((h, answer)) = answers.try_recv() {
            menu.set_answer(h, answer);
        }
    };
    let wake = Wake {
        fd: wake_r.as_raw_fd(),
        ready: &mut ready,
    };
    let choice = run_menu(&mut menu, "", Some(wake), &mut |menu, h, name| {
        end_on(args, &names[h], menu, h, name)
    });
    let (h, target, force) = match choice {
        Ok(Choice::Attach { host, name, force }) => (host, Target::Named(name), force),
        Ok(Choice::New { host }) => (host, Target::New, false),
        Ok(Choice::Leave(c)) => return c,
        // Ended in the menu, which goes on.
        Ok(Choice::Kill { .. }) => unreachable!(),
        Err(e) => {
            eprintln!("acs: {e}");
            return code::ERROR;
        }
    };
    // As `acs <alias> <session>`: the alias resolved again, its key and
    // user, and the ordinary session call, which may prompt.
    let mut picked = args.clone();
    picked.list = false;
    picked.target = target;
    picked.force |= force;
    if let Err(e) = client::resolve_alias(&mut picked, &names[h]) {
        eprintln!("acs: {e}");
        return code::UNREACHABLE;
    }
    client::run(picked, None)
}

/// Ask every alias in parallel, as `acs list` does; each answer comes on
/// the channel as `(index, answer)`, with a byte on `wake`.
fn ask_every_host(
    args: &ClientArgs,
    names: &[String],
    wake: Arc<OwnedFd>,
) -> mpsc::Receiver<(usize, Answer)> {
    let (tx, rx) = mpsc::channel();
    // Nobody can type a password into several ssh at once (Call::Batch),
    // so a host gets the redial's limit rather than the first connection's.
    let timeout = client::answer_timeout(true);
    for (h, name) in names.iter().enumerate() {
        let (tx, wake, args, name) = (tx.clone(), wake.clone(), args.clone(), name.clone());
        // Not joined: the menu may be left before a slow host answers.
        std::thread::spawn(move || {
            let answer = to_answer(&name, list::ask(&args, &name, timeout));
            if tx.send((h, answer)).is_ok() {
                let _ = sys::write_all(wake.as_raw_fd(), b"!");
            }
        });
    }
    rx
}

/// A host's listing as the menu shows it.
fn to_answer(name: &str, listed: Result<Option<Vec<StatusInfo>>, Failure>) -> Answer {
    match listed {
        Ok(Some(s)) => Answer::Sessions(s),
        Ok(None) => Answer::Line(list::not_installed(name)),
        Err(Failure::Unreachable(e)) => Answer::Line(format!("{name}: {e}")),
        Err(f) => Answer::Line(f.message(name)),
    }
}

/// End `session` on alias `alias` (menu host `h`) over a short
/// `_proxy --pick` call of its own, in BatchMode since the menu holds the
/// terminal, and show the host's sessions left.
fn end_on(args: &ClientArgs, alias: &str, menu: &mut Menu, h: usize, session: &str) {
    menu.set_note(format!("ending session '{session}' on {alias}…"));
    let _ = draw(menu, "");
    let mut on = args.clone();
    let note = match client::resolve_alias(&mut on, alias) {
        Err(e) => format!("{alias}: {e}"),
        Ok(()) => match open_pick(&on, Call::Batch, client::answer_timeout(true)) {
            Ok(Some((mut pick, _))) => {
                let note = match pick.end(session) {
                    Ok(a) => ended(menu, h, session, a),
                    Err(f) => f.message(alias),
                };
                pick.link.close();
                note
            }
            Ok(None) => list::not_installed(alias),
            Err(f) => f.message(alias),
        },
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

    #[test]
    fn a_hosts_listing_becomes_its_menu_answer() {
        assert_eq!(
            to_answer("nas", Ok(Some(vec![info("a")]))),
            Answer::Sessions(vec![info("a")])
        );
        assert_eq!(
            to_answer("pi", Ok(None)),
            Answer::Line(list::not_installed("pi"))
        );
        assert_eq!(
            to_answer("old", Err(Failure::Unreachable("no answer".into()))),
            Answer::Line("old: no answer".into())
        );
        assert_eq!(
            to_answer("lab", Err(Failure::BadReply("junk".into()))),
            Answer::Line("bad reply from lab: junk".into())
        );
    }
}
