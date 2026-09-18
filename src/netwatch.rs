//! Noticing that the local network changed (DESIGN §5.3): a Wi-Fi switch or
//! a wake from sleep is a good moment to redial at once instead of waiting
//! out the backoff.
//!
//! - macOS: a `PF_ROUTE` socket, keeping only address and interface
//!   messages (it also reports ARP cache churn, which is not a change).
//! - Linux: a `NETLINK_ROUTE` socket subscribed to address and link groups.
//! - Tests: `ACS_NETWATCH_FIFO=<path>` watches a FIFO instead.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::sys;

pub struct NetWatch {
    fd: OwnedFd,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fifo,
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Route,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Netlink,
}

impl NetWatch {
    /// Start watching; `None` where the platform offers nothing cheap.
    pub fn new() -> Option<NetWatch> {
        if let Some(path) = std::env::var_os("ACS_NETWATCH_FIFO") {
            use std::os::unix::fs::OpenOptionsExt;
            // Read-write so the FIFO never reports end of file.
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .ok()?;
            return Some(NetWatch {
                fd: f.into(),
                kind: Kind::Fifo,
            });
        }
        Self::platform().ok()
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn platform() -> io::Result<NetWatch> {
        // SAFETY: plain socket creation.
        let fd =
            sys::cvt(unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC) })?;
        // SAFETY: fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        sys::set_cloexec(fd.as_raw_fd())?;
        sys::set_nonblocking(fd.as_raw_fd(), true)?;
        Ok(NetWatch {
            fd,
            kind: Kind::Route,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn platform() -> io::Result<NetWatch> {
        // SAFETY: plain socket creation.
        let fd = sys::cvt(unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        })?;
        // SAFETY: fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: zeroed sockaddr_nl is valid; we fill the fields we need.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups =
            (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR) as u32;
        sys::cvt(unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        })?;
        Ok(NetWatch {
            fd,
            kind: Kind::Netlink,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drain pending messages; true if any was a real network change.
    pub fn changed(&self) -> bool {
        let mut buf = [0u8; 8192];
        let mut changed = false;
        while let Ok(n) = sys::read(self.fd(), &mut buf) {
            if n == 0 {
                break;
            }
            changed |= match self.kind {
                Kind::Fifo => true,
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                Kind::Route => route_messages_matter(&buf[..n]),
                // Only the subscribed groups arrive: all of them matter.
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Kind::Netlink => true,
            };
        }
        changed
    }
}

/// BSD routing messages start with `rtm_msglen: u16, rtm_version: u8,
/// rtm_type: u8`. Address and interface events matter; route churn (ARP,
/// neighbour discovery) does not.
pub fn route_messages_matter(mut buf: &[u8]) -> bool {
    const RTM_NEWADDR: u8 = 0xc;
    const RTM_DELADDR: u8 = 0xd;
    const RTM_IFINFO: u8 = 0xe;
    while buf.len() >= 4 {
        let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
        if matches!(buf[3], RTM_NEWADDR | RTM_DELADDR | RTM_IFINFO) {
            return true;
        }
        if len < 4 || len > buf.len() {
            break;
        }
        buf = &buf[len..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: u8, len: u16) -> Vec<u8> {
        let mut m = len.to_ne_bytes().to_vec();
        m.push(5);
        m.push(kind);
        m.resize(len as usize, 0);
        m
    }

    #[test]
    fn route_churn_is_not_a_change() {
        // RTM_ADD (1), RTM_DELETE (2), RTM_RESOLVE (0xb): ARP and friends.
        let churn = [msg(1, 92), msg(2, 92), msg(0xb, 92)].concat();
        assert!(!route_messages_matter(&churn));
        let mut with_addr = churn.clone();
        with_addr.extend(msg(0xc, 20));
        assert!(route_messages_matter(&with_addr));
        assert!(route_messages_matter(&msg(0xe, 112)));
        assert!(!route_messages_matter(&[1, 2]));
    }

    #[test]
    fn fifo_injection() {
        let t = crate::testutil::TempDir::new();
        let fifo = t.path().join("net");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Only this test touches the variable, and it reads it right away.
        std::env::set_var("ACS_NETWATCH_FIFO", &fifo);
        let w = NetWatch::new().unwrap();
        std::env::remove_var("ACS_NETWATCH_FIFO");
        assert!(!w.changed());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
            .unwrap();
        assert!(w.changed());
        assert!(!w.changed());
    }

    #[test]
    fn platform_watcher_opens() {
        assert!(NetWatch::platform().is_ok());
    }
}
