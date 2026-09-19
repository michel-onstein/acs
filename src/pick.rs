//! What a plain `acs [user@]<host>` attaches to (DESIGN §4.4): the host's
//! sessions are listed first; with none detached a session is created,
//! otherwise the user picks one from the menu (`menu.rs`), ends some, or
//! leaves.

use std::io;
use std::os::fd::{AsRawFd, RawFd};

use crate::cli::{ClientArgs, Target};
use crate::client::{self, code};
use crate::list;
use crate::menu::{Choice, Menu};
use crate::proto::StatusInfo;
use crate::ssh::Call;
use crate::sys;
use crate::tty::{self, AltScreen, RawMode};

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;

/// Settle a [`Target::Pick`]: to the session the user picked (with `force`
/// set for a takeover they confirmed), or to a new session. `Err` is the
/// exit status when they left the menu or the host could not be asked. The
/// host is the one `args` was resolved to, so the list and the attach reach
/// the same machine.
pub fn choose(args: &mut ClientArgs) -> Result<(), u8> {
    let Target::Pick(default) = args.target.clone() else {
        return Ok(());
    };
    // No terminal for a menu: the default session, as ever.
    if !(sys::isatty(STDIN) && sys::isatty(STDOUT)) {
        args.target = Target::Named(default);
        return Ok(());
    }
    let host = args.host_name().to_string();
    let sessions = match list::query(args, Call::Side, client::answer_timeout(false)) {
        Ok(Some(s)) => s,
        // acs is not installed there yet: the attach installs it, and there
        // is nothing to pick.
        Ok(None) => Vec::new(),
        Err(f) => {
            eprintln!("acs: {}", f.message(&host));
            return Err(f.code());
        }
    };
    if !sessions.iter().any(|s| !s.attached) {
        args.target = new_session(&sessions, &default);
        return Ok(());
    }
    let mut menu = Menu::new(sessions, args.force);
    match run_menu(args, &host, &mut menu) {
        Ok(Choice::Attach { name, force }) => {
            args.target = Target::Named(name);
            args.force |= force;
            Ok(())
        }
        Ok(Choice::New) => {
            args.target = new_session(menu.sessions(), &default);
            Ok(())
        }
        Ok(Choice::Leave(c)) => Err(c),
        // Ended in the menu, which goes on.
        Ok(Choice::Kill(_)) => unreachable!(),
        Err(e) => {
            eprintln!("acs: {e}");
            Err(code::ERROR)
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
fn run_menu(args: &ClientArgs, host: &str, menu: &mut Menu) -> io::Result<Choice> {
    tty::install_emergency_restore()?;
    let signals = sys::signals::install(&[libc::SIGWINCH])?;
    let _ = sys::signals::ignore(libc::SIGPIPE);
    let mut raw = RawMode::enter(STDIN)?;
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
            Some(Choice::Kill(name)) => end(args, host, menu, &mut raw, &name),
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

/// End `name` on the host (`_proxy --kill`, which answers with the sessions
/// left) and show the menu with those.
fn end(args: &ClientArgs, host: &str, menu: &mut Menu, raw: &mut RawMode, name: &str) {
    menu.set_note(format!("ending session '{name}'…"));
    let _ = draw(menu, host);
    // Cooked while ssh may ask for a password; the next draw repaints over
    // whatever it said.
    let _ = raw.suspend();
    let answer = list::side_call(
        args,
        &["--kill", name],
        Call::Side,
        client::answer_timeout(false),
    );
    let _ = raw.resume();
    let note = match answer {
        Ok(Some(a)) => {
            let gone = !a.sessions.iter().any(|s| s.name == name);
            menu.set_sessions(a.sessions);
            match (a.error, gone) {
                (Some(e), _) => e,
                (None, true) => format!("session '{name}' ended"),
                (None, false) => format!("session '{name}' is still there"),
            }
        }
        Ok(None) => format!("acs {} is not installed on {host}", crate::VERSION),
        Err(f) => f.message(host),
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
