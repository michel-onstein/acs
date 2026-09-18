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
        assert!(Flock::try_lock(&p).unwrap().is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_u64(), random_u64());
    }
}
