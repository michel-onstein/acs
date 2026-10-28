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

pub fn geteuid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether this process is running with an effective user other than the
/// one who started it — `sudo`, or a setuid binary. Environment variables
/// then come from one user and the privileges from another, so anything
/// that decides what gets executed must not read them (acs-95w).
pub fn privileges_dropped() -> bool {
    getuid() != geteuid()
}

pub fn getpid() -> u32 {
    // SAFETY: getpid cannot fail.
    unsafe { libc::getpid() as u32 }
}

/// Run one `getpw*_r` lookup with a buffer that outlives it, and convert
/// the entry before the buffer goes away. `call` gets the out-parameters
/// in the order `getpwuid_r` and `getpwnam_r` both take them.
fn getpw<T>(
    call: impl FnOnce(
        *mut libc::passwd,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
    convert: unsafe fn(&libc::passwd) -> Option<T>,
) -> Option<T> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter; buf outlives the call.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = call(&mut pw, buf.as_mut_ptr(), buf.len(), &mut result);
    if r != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success every non-null string field of pw points into
    // buf, which is still alive, and is NUL-terminated.
    unsafe { convert(&pw) }
}

/// A string field of a passwd entry; `None` when the field is null, which
/// an NSS backend may hand back for an empty or missing value (acs-cwmy).
///
/// # Safety
/// A non-null `p` must point to a NUL-terminated string that outlives the
/// call.
unsafe fn pw_str(p: *const libc::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null, and NUL-terminated by the caller's contract.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// The login name of a passwd entry.
///
/// # Safety
/// As [`pw_str`], for every string field of `pw`.
unsafe fn pw_login(pw: &libc::passwd) -> Option<String> {
    // SAFETY: forwarded from the caller.
    unsafe { pw_str(pw.pw_name) }
}

/// `(uid, gid, home)` of a passwd entry; `None` without a home.
///
/// # Safety
/// As [`pw_str`], for every string field of `pw`.
unsafe fn pw_ids_home(pw: &libc::passwd) -> Option<(u32, u32, std::path::PathBuf)> {
    // SAFETY: forwarded from the caller.
    let home = unsafe { pw_str(pw.pw_dir) }?;
    Some((pw.pw_uid, pw.pw_gid, home.into()))
}

/// Login name for a uid, if the password database knows it.
pub fn user_name(uid: u32) -> Option<String> {
    getpw(
        // SAFETY: the out-parameters come from getpw and are valid for it.
        |pw, buf, len, result| unsafe { libc::getpwuid_r(uid, pw, buf, len, result) },
        pw_login,
    )
}

/// `(uid, gid, home)` of a login name, if the password database knows it
/// and gives it a home.
pub fn user_by_name(name: &str) -> Option<(u32, u32, std::path::PathBuf)> {
    let c = std::ffi::CString::new(name).ok()?;
    getpw(
        // SAFETY: c outlives the call; the out-parameters come from getpw.
        |pw, buf, len, result| unsafe { libc::getpwnam_r(c.as_ptr(), pw, buf, len, result) },
        pw_ids_home,
    )
}

/// A path that runs this very binary. On Linux `/proc/self/exe` keeps
/// working after the file was replaced (an install renamed a new build over
/// it), when `current_exe()` would name a path that no longer exists.
pub fn self_exe() -> io::Result<std::path::PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let p = std::path::Path::new("/proc/self/exe");
        if p.exists() {
            return Ok(p.to_path_buf());
        }
    }
    std::env::current_exe()
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

/// A name nobody can predict, for a temporary file or directory: 16 hex
/// characters of `/dev/urandom` (acs-721).
///
/// The process id was used for some of these, which anyone can guess. That
/// matters because `acs upgrade` and the installer tell the user to re-run
/// under `sudo` into `/usr/local/bin` and `/usr/local/lib/acs`, which on
/// Intel Homebrew and hand-managed hosts are routinely writable by a
/// non-root user: pre-creating the temp name as a symlink turns the
/// root-run write into an overwrite of whatever it points at.
pub fn random_token() -> String {
    format!("{:016x}", random_u64())
}

/// Create `path` for writing, failing if anything is already there.
///
/// `create_new` is `O_CREAT|O_EXCL`, which refuses to follow a symlink at
/// the final component, so a name planted ahead of us is an error rather
/// than a write through to its target (acs-721).
pub fn create_new(path: &std::path::Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// [`create_new`], with exactly `mode` whatever the umask (acs-g4uc).
///
/// The mode `open` is given is masked by the umask — 077 leaves an
/// executable 0700 (acs-28b) — so it is set again through the handle,
/// which cannot be redirected to another file. A failure there removes
/// the file again: it is ours, made a moment ago.
pub fn create_new_mode(path: &std::path::Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    fchmod(f.as_raw_fd(), mode).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })?;
    Ok(f)
}

/// An unpredictable name beside `path`: `.<name><tag>.<token>`, for a file
/// that is renamed over it (acs-721).
fn temp_beside(path: &std::path::Path, tag: &str) -> io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(name);
    tmp.push(tag);
    tmp.push(".");
    tmp.push(random_token());
    Ok(path.with_file_name(tmp))
}

/// Replace `path` with `bytes` at exactly `mode`, so that it is the old
/// file or the new one and never half of either (acs-g4uc).
///
/// Written under a fresh name beside it by [`create_new_mode`], so a name
/// planted ahead of us is refused rather than written through, and renamed
/// over it. The rename replaces `path` itself: a symlink there is replaced
/// by a regular file, so a caller that edits what a link leads to resolves
/// it first. On any failure after the temporary file is made, it is
/// removed.
pub fn write_atomic(path: &std::path::Path, mode: u32, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = temp_beside(path, "")?;
    let mut f = create_new_mode(&tmp, mode)?;
    let written = f.write_all(bytes);
    drop(f);
    written
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Point the symlink `link` at `target`, atomically: a fresh link beside it
/// renamed over it, so `link` is never missing (acs-g4uc). `symlink` fails
/// on a name already taken rather than following it, and the temporary
/// link is removed when the rename fails.
pub fn symlink_atomic(target: &std::path::Path, link: &std::path::Path) -> io::Result<()> {
    let tmp = temp_beside(link, ".link")?;
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Open `path` as a directory without following a symlink at its last
/// component, for checking and changing it through the handle rather than
/// by name (acs-hjk).
pub fn open_dir_nofollow(path: &std::path::Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid NUL-terminated path; the fd is wrapped below.
    let fd = retry(|| unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: open returned a fresh descriptor we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Set the mode of whatever `fd` refers to. Unlike a `chmod` by path this
/// cannot be redirected between the check and the change (acs-hjk).
///
/// Not for sockets: macOS refuses `fchmod` on one with `EINVAL`. Create
/// those under a [`Umask`] instead.
pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    // SAFETY: fchmod on a descriptor we own.
    retry(|| unsafe { libc::fchmod(fd, mode as libc::mode_t) })?;
    Ok(())
}

/// The process umask, put back when this is dropped (acs-hjk).
///
/// The way to give a file a mode it cannot briefly lack is to create it
/// that way. That matters for a unix socket, which is created by `bind`
/// and which macOS will not let `fchmod` touch afterwards.
pub struct Umask(libc::mode_t);

impl Umask {
    /// Set the umask to `mask` until the guard is dropped.
    pub fn set(mask: u32) -> Umask {
        // SAFETY: umask cannot fail and returns the previous value.
        Umask(unsafe { libc::umask(mask as libc::mode_t) })
    }
}

impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: as above, restoring what we found.
        unsafe { libc::umask(self.0) };
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `HH:MM` in local time for Unix seconds `t`.
// libc marks musl's `time_t` deprecated ahead of its 64-bit change; the
// cast is right either way.
#[allow(deprecated)]
pub fn local_hhmm(t: u64) -> String {
    let secs = t as libc::time_t;
    // SAFETY: zeroed tm is a valid out-parameter for localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "?".into();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Set a path's access and modification time to now.
pub fn touch(path: &std::path::Path) -> io::Result<()> {
    set_mtime(path, unix_now())
}

/// Set a path's access and modification time to Unix seconds `t`.
pub fn set_mtime(path: &std::path::Path, t: u64) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let tv = libc::timeval {
        tv_sec: t as _,
        tv_usec: 0,
    };
    let times = [tv, tv];
    cvt(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) })?;
    Ok(())
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

/// [`tcsetattr`], discarding the input not read yet.
pub fn tcsetattr_flush(fd: RawFd, t: &libc::termios) -> io::Result<()> {
    retry(|| unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, t) })?;
    Ok(())
}

/// Discard the input not read yet.
pub fn flush_input(fd: RawFd) -> io::Result<()> {
    cvt(unsafe { libc::tcflush(fd, libc::TCIFLUSH) })?;
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

/// Make `pgrp` the foreground process group of the terminal on `fd`.
/// `SIGTTOU` is held off for the call, so a caller in the background — acs
/// taking its terminal back after ssh's prompt (acs-8ax) — is not stopped
/// by asking.
pub fn tcsetpgrp(fd: RawFd, pgrp: i32) -> io::Result<()> {
    // SAFETY: the sets are initialised by sigemptyset before use.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        let r = libc::tcsetpgrp(fd, pgrp);
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        cvt(r)?;
    }
    Ok(())
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

/// `poll(2)` that sits out signals: an interrupted call is resumed for the
/// time left, so `Ok(0)` really means the timeout expired with nothing
/// ready. A negative `timeout_ms` waits without a deadline, as `poll(2)`
/// and [`poll`] do (acs-e43p: it used to overflow `Instant` and panic).
pub fn poll_retry(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    let deadline = u64::try_from(timeout_ms)
        .ok()
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    loop {
        let left = match deadline {
            Some(d) => poll_timeout(d.saturating_duration_since(std::time::Instant::now())),
            None => -1,
        };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, left) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// `d` as a `poll(2)` timeout in milliseconds, clamped to `i32::MAX`: a
/// bare `as i32` wraps a wait of 2^31 ms or more to a negative, which
/// `poll` reads as no timeout at all (acs-wjj9).
pub fn poll_timeout(d: std::time::Duration) -> i32 {
    d.as_millis().min(i32::MAX as u128) as i32
}

/// The `poll(2)` timeout until the soonest of `deadline`, in [`now_ms`]
/// milliseconds seen at `now`: `-1` (no timeout) for none, `0` for one
/// already past, clamped to `i32::MAX`.
pub fn poll_timeout_ms(deadline: Option<u64>, now: u64) -> i32 {
    deadline.map_or(-1, |d| d.saturating_sub(now).min(i32::MAX as u64) as i32)
}

/// Wait until `fd` is readable (`true`) or `deadline` passes (`false`). A
/// signal that cuts the wait short is sat out: the wait resumes for the time
/// left, so `false` really means the deadline.
pub fn wait_readable(fd: RawFd, deadline: std::time::Instant) -> io::Result<bool> {
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut p = [pollfd(fd, libc::POLLIN)];
        if poll(&mut p, poll_timeout(left))? > 0 {
            return Ok(true);
        }
    }
}

/// A process group to signal from `tcgetpgrp`: never 0 or negative (a pty
/// with no foreground group reports 0, and `kill(-0, …)` would hit our own
/// group) and never `own`.
pub fn valid_pgrp(p: i32, own: i32) -> Option<i32> {
    (p > 0 && p != own).then_some(p)
}

pub fn getpgrp() -> i32 {
    unsafe { libc::getpgrp() }
}

pub fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

/// A poll entry for `fd`, or none (`fd -1`) when there is nothing to wait
/// for: a hung-up pipe, socket or pty reports POLLHUP whatever `events`
/// asks, which spins a loop whose other side is stuck — the proxy's relay
/// (acs-wza) and the master's pty while the ring is full (acs-gkl).
pub fn wait_for(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    pollfd(if events == 0 { -1 } else { fd }, events)
}

/// `cmd.spawn()`, one thread at a time. Where there is no `pipe2` (macOS),
/// the standard library makes a child's pipes with `pipe` and marks them
/// close-on-exec a moment later; a child another thread forks in between
/// keeps them open, and the first child's output then never reaches end of
/// file while the second one lives. `acs list` dials every alias at once.
pub fn spawn(cmd: &mut std::process::Command) -> io::Result<std::process::Child> {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    cmd.spawn()
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

/// Close every inherited descriptor from 3 up except `keep`. Daemons do
/// this so a descriptor leaked into them by a racing fork elsewhere (a
/// pipe's write end, say) cannot keep someone else's pipe open for the
/// daemon's whole life.
pub fn close_inherited(keep: &[RawFd]) {
    let Ok(dir) = std::fs::read_dir("/dev/fd") else {
        return;
    };
    let fds: Vec<RawFd> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    for fd in fds {
        if fd > 2 && !keep.contains(&fd) {
            // SAFETY: closing a descriptor number; EBADF (the listing's own
            // descriptor, already closed) is harmless.
            unsafe { libc::close(fd) };
        }
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

/// Whether child `pid` has been stopped by a signal since it was last
/// continued. An exited child is left to be reaped by whoever waits for it.
pub fn stopped(pid: i32) -> bool {
    // SAFETY: a zeroed siginfo_t is a valid out-parameter, and si_pid stays
    // 0 when no child is in the state asked for.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WSTOPPED | libc::WNOHANG,
        );
        r == 0 && info.si_pid() != 0
    }
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

    /// acs-721: a temporary name planted ahead of us must be an error, not
    /// a write through to whatever it points at. `acs upgrade` and the
    /// installer both tell the user to re-run under sudo into directories
    /// a non-root user can often write, so this would be an arbitrary
    /// overwrite as root.
    #[test]
    fn create_new_refuses_a_name_that_is_already_there() {
        let t = crate::testutil::TempDir::new();
        let secret = t.path().join("precious");
        std::fs::write(&secret, b"do not touch").unwrap();

        // A symlink waiting at the name we are about to use.
        let planted = t.path().join(".acs.upgrade.deadbeef");
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        let e = create_new(&planted).expect_err("wrote through a planted symlink");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&secret).unwrap(), b"do not touch");

        // A plain file in the way is refused just the same.
        let taken = t.path().join(".acs.upgrade.cafe");
        std::fs::write(&taken, b"someone else's").unwrap();
        assert_eq!(
            create_new(&taken).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&taken).unwrap(), b"someone else's");

        // And a free name works.
        let fresh = t.path().join(format!(".acs.upgrade.{}", random_token()));
        create_new(&fresh).expect("a free name");
        assert!(fresh.exists());
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// The names in `dir`, sorted: what a failed write must leave as it was.
    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// acs-g4uc: the three helpers under a umask that would widen a
    /// default mode (002, Ubuntu's with user-private groups) and one that
    /// would narrow it (077). The umask is the whole process's, so each is
    /// run in a child test process of its own rather than racing every
    /// other test here.
    #[test]
    fn the_atomic_helpers_ignore_the_umask() {
        const VAR: &str = "ACS_TEST_SYS_UMASK";
        if let Some(mask) = std::env::var_os(VAR) {
            let mask = u32::from_str_radix(mask.to_str().unwrap(), 8).unwrap();
            let _mask = Umask::set(mask);
            atomic_helpers_under_the_current_umask(mask);
            return;
        }
        let me = std::env::current_exe().unwrap();
        for mask in ["002", "077"] {
            let out = std::process::Command::new(&me)
                .args([
                    "sys::tests::the_atomic_helpers_ignore_the_umask",
                    "--exact",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(VAR, mask)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && text.contains("1 passed"),
                "under umask {mask}:\n{text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn atomic_helpers_under_the_current_umask(mask: u32) {
        let t = crate::testutil::TempDir::new();
        // A plain create shows the umask really is in force.
        let plain = t.path().join("plain");
        drop(create_new(&plain).unwrap());
        assert_eq!(mode_of(&plain), 0o666 & !mask);

        let exe = t.path().join("exe");
        drop(create_new_mode(&exe, 0o755).unwrap());
        assert_eq!(mode_of(&exe), 0o755, "umask {mask:o}");

        let state = t.path().join("state");
        write_atomic(&state, 0o600, b"one").unwrap();
        assert_eq!(mode_of(&state), 0o600, "umask {mask:o}");
        write_atomic(&state, 0o644, b"two").unwrap();
        assert_eq!(mode_of(&state), 0o644, "umask {mask:o}");
        assert_eq!(std::fs::read(&state).unwrap(), b"two");

        let link = t.path().join("link");
        symlink_atomic(&exe, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), exe);
        assert_eq!(
            names(t.path()),
            ["exe", "link", "plain", "state"],
            "a temporary name was left behind"
        );
    }

    /// acs-g4uc: `create_new_mode` refuses a name already there, a planted
    /// symlink above all, exactly as [`create_new`] does (acs-721).
    #[test]
    fn create_new_mode_refuses_a_name_that_is_already_there() {
        let t = crate::testutil::TempDir::new();
        let secret = t.path().join("precious");
        std::fs::write(&secret, b"do not touch").unwrap();
        let secret_mode = mode_of(&secret);
        let planted = t.path().join("planted");
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        let e = create_new_mode(&planted, 0o777).expect_err("wrote through a planted symlink");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&secret).unwrap(), b"do not touch");
        assert_eq!(mode_of(&secret), secret_mode, "chmodded through the link");
        assert!(std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// acs-g4uc: `write_atomic` replaces the path itself, and a failure —
    /// before the temporary file exists or after — leaves the directory as
    /// it was, with no temporary file behind.
    #[test]
    fn write_atomic_replaces_whole_or_leaves_nothing_behind() {
        let t = crate::testutil::TempDir::new();
        let f = t.path().join("f");
        std::fs::write(&f, b"old").unwrap();
        write_atomic(&f, 0o600, b"new").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
        assert_eq!(mode_of(&f), 0o600);

        // A symlink at the path is replaced by a file, not written through:
        // a caller that means the link's target resolves it first.
        let target = t.path().join("target");
        std::fs::write(&target, b"kept").unwrap();
        let link = t.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, 0o600, b"file").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");

        // The rename fails: a non-empty directory is in the way.
        let dir = t.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inside"), b"").unwrap();
        let before = names(t.path());
        write_atomic(&dir, 0o600, b"x").expect_err("renamed over a directory");
        assert_eq!(names(t.path()), before, "a temporary file was left behind");

        // The create fails: no directory to write in.
        write_atomic(&t.path().join("missing/f"), 0o600, b"x").expect_err("no directory");
        assert_eq!(names(t.path()), before);

        // A path with no file name has no temporary name either.
        assert_eq!(
            write_atomic(std::path::Path::new("/"), 0o600, b"x")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// acs-g4uc: `symlink_atomic` repoints an existing link, and when the
    /// rename fails the temporary link is gone again.
    #[test]
    fn symlink_atomic_repoints_or_leaves_nothing_behind() {
        let t = crate::testutil::TempDir::new();
        let link = t.path().join("acs");
        symlink_atomic(std::path::Path::new("one"), &link).unwrap();
        symlink_atomic(std::path::Path::new("two"), &link).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("two")
        );

        let dir = t.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inside"), b"").unwrap();
        let before = names(t.path());
        symlink_atomic(std::path::Path::new("x"), &dir).expect_err("renamed over a directory");
        assert_eq!(names(t.path()), before, "a temporary link was left behind");
    }

    /// acs-721: the names are not guessable, and not the process id.
    #[test]
    fn a_random_token_is_wide_and_not_the_pid() {
        let a = random_token();
        assert_eq!(a.len(), 16, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, random_token(), "twice the same token");
        assert!(!a.contains(&getpid().to_string()), "{a} carries the pid");
    }

    #[test]
    fn current_user_has_a_name() {
        assert!(user_name(getuid()).is_some_and(|n| !n.is_empty()));
    }

    /// acs-cwmy: an NSS backend can return an entry whose home or name is
    /// null. That is "no such user", not `CStr::from_ptr(null)`.
    #[test]
    fn a_passwd_entry_with_a_null_field_converts_to_none() {
        // SAFETY: a zeroed passwd has every pointer null, the case under
        // test; the strings set below are 'static and NUL-terminated.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        pw.pw_uid = 501;
        pw.pw_gid = 20;
        assert_eq!(unsafe { pw_ids_home(&pw) }, None);
        assert_eq!(unsafe { pw_login(&pw) }, None);

        pw.pw_dir = c"/home/someone".as_ptr() as *mut libc::c_char;
        pw.pw_name = c"someone".as_ptr() as *mut libc::c_char;
        assert_eq!(
            unsafe { pw_ids_home(&pw) },
            Some((501, 20, "/home/someone".into()))
        );
        assert_eq!(unsafe { pw_login(&pw) }.as_deref(), Some("someone"));
    }

    #[test]
    fn current_user_resolves_by_name() {
        let name = user_name(getuid()).expect("own name");
        let (uid, _, home) = user_by_name(&name).expect("own entry");
        assert_eq!(uid, getuid());
        assert!(home.is_absolute(), "{}", home.display());
    }

    /// Regression (acs-ov1): a pty with no foreground group reports 0, and
    /// `kill(-0, SIGKILL)` would kill the master's own group.
    #[test]
    fn only_real_foreign_process_groups_are_signalled() {
        assert_eq!(valid_pgrp(0, 100), None);
        assert_eq!(valid_pgrp(-1, 100), None);
        assert_eq!(valid_pgrp(100, 100), None);
        assert_eq!(valid_pgrp(4321, 100), Some(4321));
    }

    /// acs-wza/acs-gkl: with nothing asked for, a hung-up descriptor is
    /// left out of the poll instead of reporting POLLHUP on every round.
    #[test]
    fn a_descriptor_with_nothing_to_wait_for_is_left_out_of_the_poll() {
        let (r, w) = pipe().unwrap();
        drop(w);
        let mut fds = [wait_for(r.as_raw_fd(), 0)];
        assert_eq!(fds[0].fd, -1);
        assert_eq!(poll(&mut fds, 0).unwrap(), 0);
        let mut fds = [wait_for(r.as_raw_fd(), libc::POLLIN)];
        assert_eq!(fds[0].fd, r.as_raw_fd());
        assert_eq!(
            poll(&mut fds, 0).unwrap(),
            1,
            "the hang-up is seen when asked"
        );
    }

    #[test]
    fn poll_retry_waits_out_the_full_timeout() {
        let (r, _w) = pipe().unwrap();
        let mut fds = [pollfd(r.as_raw_fd(), libc::POLLIN)];
        let t = std::time::Instant::now();
        assert_eq!(poll_retry(&mut fds, 30).unwrap(), 0);
        assert!(t.elapsed() >= std::time::Duration::from_millis(29));
    }

    /// acs-e43p: a negative timeout means no deadline, as in `poll(2)` and
    /// [`poll`]; it used to overflow `Instant + Duration` and panic.
    #[test]
    fn poll_retry_without_a_deadline_waits_for_readiness() {
        let (r, w) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let n = unsafe { libc::write(w.as_raw_fd(), b"x".as_ptr().cast(), 1) };
            assert_eq!(n, 1);
            w
        });
        let mut fds = [pollfd(r.as_raw_fd(), libc::POLLIN)];
        assert_eq!(poll_retry(&mut fds, -1).unwrap(), 1);
        assert_ne!(fds[0].revents & libc::POLLIN, 0);
        drop(writer.join().unwrap());
    }

    /// acs-wjj9: a wait past `i32::MAX` ms clamps instead of wrapping to a
    /// negative timeout, which `poll` would treat as "block forever".
    #[test]
    fn a_wait_past_i32_max_ms_clamps_the_poll_timeout() {
        use std::time::Duration;
        assert_eq!(poll_timeout(Duration::from_millis(1500)), 1500);
        assert_eq!(
            poll_timeout(Duration::from_millis(i32::MAX as u64)),
            i32::MAX
        );
        assert_eq!(poll_timeout(Duration::from_millis(1 << 31)), i32::MAX);
        assert_eq!(poll_timeout(Duration::from_millis(u64::MAX)), i32::MAX);
    }

    /// A millisecond deadline: none waits for ever, a past one not at all,
    /// and one 2^31 ms or more away clamps rather than wrapping negative.
    #[test]
    fn a_millisecond_deadline_becomes_a_clamped_poll_timeout() {
        assert_eq!(poll_timeout_ms(None, 1000), -1);
        assert_eq!(poll_timeout_ms(Some(1500), 1000), 500);
        assert_eq!(poll_timeout_ms(Some(900), 1000), 0);
        assert_eq!(poll_timeout_ms(Some(1000 + (1 << 31)), 1000), i32::MAX);
        assert_eq!(poll_timeout_ms(Some(u64::MAX), 0), i32::MAX);
    }

    /// A signal arriving mid-wait is sat out: `wait_readable` neither
    /// reports the deadline early nor readiness that is not there, and still
    /// sees data that comes after the signals.
    #[test]
    fn wait_readable_sits_out_signals_until_data_or_the_deadline() {
        use std::time::{Duration, Instant};
        extern "C" fn nothing(_: libc::c_int) {}
        // SIGURG is ignored by default and used nowhere else; a handler
        // without SA_RESTART makes it interrupt `poll` with EINTR.
        signals::handle(libc::SIGURG, nothing as *const () as usize, 0).unwrap();
        let me = unsafe { libc::pthread_self() } as usize;
        let prod = |n: u32| {
            std::thread::spawn(move || {
                for _ in 0..n {
                    std::thread::sleep(Duration::from_millis(10));
                    unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGURG) };
                }
            })
        };

        let (r, w) = pipe().unwrap();
        let t = Instant::now();
        let p = prod(5);
        assert!(!wait_readable(r.as_raw_fd(), t + Duration::from_millis(150)).unwrap());
        assert!(
            t.elapsed() >= Duration::from_millis(149),
            "{:?}",
            t.elapsed()
        );
        p.join().unwrap();

        let p = prod(3);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let n = unsafe { libc::write(w.as_raw_fd(), b"x".as_ptr().cast(), 1) };
            assert_eq!(n, 1);
            w
        });
        assert!(wait_readable(r.as_raw_fd(), Instant::now() + Duration::from_secs(10)).unwrap());
        p.join().unwrap();
        drop(writer.join().unwrap());
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
