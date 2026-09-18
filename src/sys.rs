//! Thin, safe wrappers over the libc calls the standard library does not
//! expose. Everything unsafe in the crate lives here.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

/// Map a `-1` return to the current `errno`.
pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Retry a call interrupted by a signal.
pub fn retry<F: FnMut() -> libc::c_int>(mut f: F) -> io::Result<libc::c_int> {
    loop {
        match cvt(f()) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

pub fn getuid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

pub fn getpid() -> u32 {
    // SAFETY: getpid cannot fail.
    unsafe { libc::getpid() as u32 }
}

/// Login name for a uid, if the password database knows it.
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter; buf outlives the call.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut result) };
    if r != 0 || result.is_null() || pw.pw_name.is_null() {
        return None;
    }
    // SAFETY: pw_name points into buf and is NUL-terminated.
    Some(
        unsafe { CStr::from_ptr(pw.pw_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for its length.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    // Short name, as a prompt would show it.
    name.split('.').next().unwrap_or("localhost").to_string()
}

/// Maximum usable length of a unix socket path on this platform.
pub fn max_socket_path() -> usize {
    // SAFETY: only the size of the field is used.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// An advisory `flock` held for as long as the value lives.
pub struct Flock {
    file: File,
}

impl Flock {
    fn open(path: &std::path::Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    /// Block until the lock is ours.
    pub fn lock(path: &std::path::Path) -> io::Result<Flock> {
        let file = Self::open(path)?;
        retry(|| unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) })?;
        Ok(Flock { file })
    }

    /// Take the lock if free; `Ok(None)` if someone else holds it.
    pub fn try_lock(path: &std::path::Path) -> io::Result<Option<Flock>> {
        let file = Self::open(path)?;
        match retry(|| unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }) {
            Ok(_) => Ok(Some(Flock { file })),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// Fill `buf` with random bytes from the kernel.
pub fn random_bytes(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback: time and pid mixed; only used for ids, never for secrets.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = t ^ ((getpid() as u64) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    random_bytes(&mut b);
    u64::from_ne_bytes(b)
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Milliseconds on a monotonic clock (the command-key detector's clock).
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

// ---- terminals -------------------------------------------------------------

pub fn isatty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

pub fn tcgetattr(fd: RawFd) -> io::Result<libc::termios> {
    // SAFETY: zeroed termios is a valid out-parameter.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::tcgetattr(fd, &mut t) })?;
    Ok(t)
}

pub fn tcsetattr(fd: RawFd, t: &libc::termios) -> io::Result<()> {
    retry(|| unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, t) })?;
    Ok(())
}

/// dtach's raw mode: no input translation, no output post-processing, no
/// echo, no canonical mode, no signals from keys, 8-bit, one byte at a time.
pub fn make_raw(orig: &libc::termios) -> libc::termios {
    let mut t = *orig;
    t.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON
        | libc::IXOFF);
    t.c_oflag &= !libc::OPOST;
    t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    t.c_cflag &= !(libc::CSIZE | libc::PARENB);
    t.c_cflag |= libc::CS8;
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
    t
}

pub fn get_winsize(fd: RawFd) -> io::Result<crate::proto::WinSize> {
    // SAFETY: zeroed winsize is a valid out-parameter.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) })?;
    Ok(crate::proto::WinSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
        xpixel: ws.ws_xpixel,
        ypixel: ws.ws_ypixel,
    })
}

pub fn set_winsize(fd: RawFd, s: &crate::proto::WinSize) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: s.rows,
        ws_col: s.cols,
        ws_xpixel: s.xpixel,
        ws_ypixel: s.ypixel,
    };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) })?;
    Ok(())
}

/// A new pseudo-terminal pair `(master, slave)`, both close-on-exec.
pub fn openpty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut m: libc::c_int = -1;
    let mut s: libc::c_int = -1;
    // SAFETY: out-parameters are valid; name/termp/winp may be null.
    cvt(unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    })?;
    // SAFETY: openpty returned two fresh descriptors we now own.
    let (m, s) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    set_cloexec(m.as_raw_fd())?;
    set_cloexec(s.as_raw_fd())?;
    Ok((m, s))
}

/// Foreground process group of the terminal on `fd`.
pub fn tcgetpgrp(fd: RawFd) -> io::Result<i32> {
    cvt(unsafe { libc::tcgetpgrp(fd) })
}

// ---- descriptors -----------------------------------------------------------

use std::os::fd::{FromRawFd, OwnedFd};

pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) })?;
    Ok(())
}

pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let flags = if on {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) })?;
    Ok(())
}

/// A close-on-exec pipe `(read, write)`.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: pipe returned two fresh descriptors we now own.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(r.as_raw_fd())?;
    set_cloexec(w.as_raw_fd())?;
    Ok((r, w))
}

/// `read(2)`, retrying on EINTR. `Ok(0)` is end of file.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// `write(2)`, retrying on EINTR; may write less than `buf`.
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Write everything, waiting for writability if the descriptor is
/// non-blocking.
pub fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match write(fd, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut p = [libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                poll(&mut p, -1)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `poll(2)`; an interrupted call returns `Ok(0)` so the caller re-checks
/// its signal pipe.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if n >= 0 {
        return Ok(n as usize);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::Interrupted {
        Ok(0)
    } else {
        Err(e)
    }
}

pub fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

/// Uid of the process on the other end of a unix socket.
pub fn peer_uid(fd: RawFd) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        cvt(unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        })?;
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        cvt(unsafe { libc::getpeereid(fd, &mut uid, &mut gid) })?;
        Ok(uid)
    }
}

// ---- processes and signals -------------------------------------------------

pub fn kill(pid: i32, sig: libc::c_int) -> io::Result<()> {
    cvt(unsafe { libc::kill(pid, sig) })?;
    Ok(())
}

/// Non-blocking `waitpid`: `Ok(Some(status))` once the child has exited.
pub fn try_wait(pid: i32) -> io::Result<Option<i32>> {
    let mut status = 0;
    let r = retry(|| unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) })?;
    Ok((r == pid).then_some(status))
}

/// Exit code a shell would report for a wait status (128+n for signals).
pub fn exit_code(status: i32) -> u8 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u8
    } else if libc::WIFSIGNALED(status) {
        (128 + libc::WTERMSIG(status)) as u8
    } else {
        1
    }
}

/// Self-pipe for signals: handlers write the signal number, the poll loop
/// reads it. Only one pipe per process.
pub mod signals {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(sig: libc::c_int) {
        let fd = WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = sig as u8;
            // SAFETY: write is async-signal-safe; errors (full pipe) are fine.
            unsafe {
                let e = *errno_location();
                libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
                *errno_location() = e;
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__errno_location()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__error()
    }

    /// Route `sigs` into a pipe; returns its non-blocking read end.
    pub fn install(sigs: &[libc::c_int]) -> io::Result<OwnedFd> {
        let (r, w) = pipe()?;
        set_nonblocking(r.as_raw_fd(), true)?;
        set_nonblocking(w.as_raw_fd(), true)?;
        let old = WRITE_FD.swap(w.as_raw_fd(), Ordering::Relaxed);
        std::mem::forget(w);
        if old >= 0 {
            unsafe { libc::close(old) };
        }
        for &s in sigs {
            handle(s, on_signal as *const () as usize, 0)?;
        }
        Ok(r)
    }

    /// Install a raw handler address (or `SIG_DFL`/`SIG_IGN`).
    pub fn handle(sig: libc::c_int, handler: usize, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: a zeroed sigaction with an empty mask is valid.
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = handler;
        sa.sa_flags = flags;
        unsafe { libc::sigemptyset(&mut sa.sa_mask) };
        cvt(unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) })?;
        Ok(())
    }

    pub fn ignore(sig: libc::c_int) -> io::Result<()> {
        handle(sig, libc::SIG_IGN, 0)
    }

    /// Drain the pipe, returning the signals received.
    pub fn drain(fd: RawFd) -> Vec<libc::c_int> {
        let mut buf = [0u8; 64];
        let mut out = Vec::new();
        while let Ok(n) = read(fd, &mut buf) {
            if n == 0 {
                break;
            }
            out.extend(buf[..n].iter().map(|&b| b as libc::c_int));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_has_a_name() {
        assert!(user_name(getuid()).is_some_and(|n| !n.is_empty()));
    }

    #[test]
    fn socket_path_limit_is_platform_sized() {
        let n = max_socket_path();
        assert!((100..=107).contains(&n), "{n}");
    }

    #[test]
    fn flock_is_exclusive_across_opens() {
        let p = std::env::temp_dir().join(format!("acs-flock-{}", getpid()));
        let held = Flock::lock(&p).unwrap();
        assert!(Flock::try_lock(&p).unwrap().is_none());
        drop(held);
        // Another test may fork while we hold the lock; its child shares the
        // descriptor until exec closes it (close-on-exec), so allow a moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while Flock::try_lock(&p).unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "lock never freed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_u64(), random_u64());
    }
}
