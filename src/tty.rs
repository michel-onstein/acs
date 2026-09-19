//! The client's local terminal (DESIGN §7): raw mode, and getting the
//! original settings back on every way out — normal return, a fatal signal,
//! or a panic (which aborts in release builds, so no destructor would run).
//! The session menu's alternate screen (DESIGN §4.4) is left the same ways.

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Once, OnceLock};

use crate::sys;

/// Terminal and settings to restore in an emergency. Written once, before
/// any handler that reads it is installed.
static SAVED_FD: AtomicI32 = AtomicI32::new(-1);
static SAVED: OnceLock<libc::termios> = OnceLock::new();
/// The terminal on which an [`AltScreen`] is showing, or -1.
static ALT_FD: AtomicI32 = AtomicI32::new(-1);

/// Back to the normal screen, with the cursor shown.
const LEAVE_ALT: &[u8] = b"\x1b[?25h\x1b[?1049l";

/// Restore the saved settings. Async-signal-safe: one `write` for the
/// screen, one `tcsetattr`.
fn emergency_restore() {
    let alt = ALT_FD.swap(-1, Ordering::AcqRel);
    if alt >= 0 {
        // SAFETY: write is async-signal-safe; the bytes are static.
        unsafe { libc::write(alt, LEAVE_ALT.as_ptr().cast(), LEAVE_ALT.len()) };
    }
    let fd = SAVED_FD.load(Ordering::Acquire);
    if let (true, Some(t)) = (fd >= 0, SAVED.get()) {
        // SAFETY: tcsetattr is async-signal-safe; `t` is immutable once set.
        unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, t) };
    }
}

extern "C" fn on_fatal(sig: libc::c_int) {
    emergency_restore();
    // Die of the same signal so the parent sees the right status.
    let _ = sys::signals::handle(sig, libc::SIG_DFL, 0);
    // SAFETY: raise is async-signal-safe.
    unsafe { libc::raise(sig) };
}

/// Install the panic hook and fatal-signal handlers that put the terminal
/// back, before entering raw mode. The session menu and the session both
/// call it; the hook is installed only once.
pub fn install_emergency_restore() -> io::Result<()> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            prev(info);
        }));
    });
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGINT] {
        sys::signals::handle(sig, on_fatal as *const () as usize, 0)?;
    }
    Ok(())
}

/// The alternate screen, with the cursor hidden, for as long as this lives
/// (the session menu). Leaving it brings back what the terminal showed
/// before; a fatal signal or a panic leaves it too.
pub struct AltScreen {
    fd: RawFd,
}

impl AltScreen {
    pub fn enter(fd: RawFd) -> io::Result<AltScreen> {
        ALT_FD.store(fd, Ordering::Release);
        sys::write_all(fd, b"\x1b[?1049h\x1b[?25l")?;
        Ok(AltScreen { fd })
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        if ALT_FD.swap(-1, Ordering::AcqRel) >= 0 {
            let _ = sys::write_all(self.fd, LEAVE_ALT);
        }
    }
}

/// Raw mode on a terminal for as long as this lives.
pub struct RawMode {
    fd: RawFd,
    orig: libc::termios,
    raw: bool,
}

impl RawMode {
    pub fn enter(fd: RawFd) -> io::Result<RawMode> {
        let orig = sys::tcgetattr(fd)?;
        // First terminal wins; the client only ever uses one.
        if SAVED.set(orig).is_ok() {
            SAVED_FD.store(fd, Ordering::Release);
        }
        let mut m = RawMode {
            fd,
            orig,
            raw: false,
        };
        m.resume()?;
        Ok(m)
    }

    /// Back to the original (cooked) settings, e.g. while ssh asks for a
    /// password on redial.
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.raw {
            sys::tcsetattr(self.fd, &self.orig)?;
            self.raw = false;
        }
        Ok(())
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if !self.raw {
            sys::tcsetattr(self.fd, &sys::make_raw(&self.orig))?;
            self.raw = true;
        }
        Ok(())
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_flags_match_dtach() {
        let (_m, s) = sys::openpty().unwrap();
        use std::os::fd::AsRawFd;
        let orig = sys::tcgetattr(s.as_raw_fd()).unwrap();
        let raw = sys::make_raw(&orig);
        assert_eq!(
            raw.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN),
            0
        );
        assert_eq!(raw.c_iflag & (libc::ICRNL | libc::IXON | libc::ISTRIP), 0);
        assert_eq!(raw.c_oflag & libc::OPOST, 0);
        assert_eq!(raw.c_cflag & libc::CSIZE, libc::CS8);
        assert_eq!(raw.c_cc[libc::VMIN], 1);
        assert_eq!(raw.c_cc[libc::VTIME], 0);
    }

    #[test]
    fn suspend_and_resume_round_trip() {
        let (_m, s) = sys::openpty().unwrap();
        use std::os::fd::AsRawFd;
        let fd = s.as_raw_fd();
        // Only the bits raw mode changes: the kernel may set others (macOS
        // sets PENDIN when canonical mode comes back).
        let bits = libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
        let lflag = || sys::tcgetattr(fd).unwrap().c_lflag & bits;
        let orig = lflag();
        {
            let mut r = RawMode::enter(fd).unwrap();
            assert_eq!(lflag(), 0);
            r.suspend().unwrap();
            assert_eq!(lflag(), orig);
            r.resume().unwrap();
            assert!(r.is_raw());
            assert_eq!(lflag(), 0);
        }
        assert_eq!(lflag(), orig);
    }
}
