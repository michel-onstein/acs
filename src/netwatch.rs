//! Noticing that the local network changed (DESIGN §5.3): a Wi-Fi switch or
//! a wake from sleep is a good moment to redial at once instead of waiting
//! out the backoff.
//!
//! Two halves, and the second is the one that decides (acs-6p8):
//!
//! 1. **A hint** that the kernel touched the network at all, which is what
//!    the socket is for:
//!    - macOS: a `PF_ROUTE` socket, keeping only address and interface
//!      messages (it also reports ARP cache churn, which is not a change);
//!    - Linux: a `NETLINK_ROUTE` socket subscribed to address and link
//!      groups;
//!    - tests: `ACS_NETWATCH_FIFO=<path>` reads a FIFO instead, where every
//!      write is one hint.
//! 2. **Whether anything changed** — the networks this machine could dial
//!    from ([`crate::netmatch::local_networks`]), compared with the ones it
//!    was on when the hint before it arrived. A hint that leaves them as
//!    they were is the kernel *mentioning* the network, not the network
//!    changing: an unrelated interface flapping, a VPN churning its routes,
//!    an IPv6 duplicate-address probe. Tests put those networks in a file
//!    with `ACS_NETWATCH_NETS=<path>`, one CIDR per line, where the client
//!    has `getifaddrs`.
//!
//! Without the second half a laptop redialled all through an outage — every
//! hint cut the backoff short — and each redial threw away what had been
//! typed into it (DESIGN §5.2).

use std::cell::RefCell;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::sys;

pub struct NetWatch {
    fd: OwnedFd,
    kind: Kind,
    /// The networks as of the last hint that was reported as a change —
    /// or as of the moment the watcher was made, so the first hint of an
    /// outage is judged like any other rather than taken on trust.
    nets: RefCell<Vec<String>>,
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
                nets: RefCell::new(networks()),
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
            nets: RefCell::new(networks()),
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
            nets: RefCell::new(networks()),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drain the pending messages; true if the network this machine dials
    /// from is not the one it was on.
    ///
    /// Draining is unconditional — the descriptor has to be emptied for the
    /// poll it woke to sleep again — but a hint is only a reason to look
    /// (acs-6p8). What makes it a change is [`networks`] differing, and the
    /// answer is remembered, so a kernel that repeats itself is answered
    /// once.
    pub fn changed(&self) -> bool {
        let mut buf = [0u8; 8192];
        let mut hinted = false;
        while let Ok(n) = sys::read(self.fd(), &mut buf) {
            if n == 0 {
                break;
            }
            hinted |= match self.kind {
                Kind::Fifo => true,
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                Kind::Route => route_messages_matter(&buf[..n]),
                // Only the subscribed groups arrive: all of them are hints.
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Kind::Netlink => true,
            };
        }
        if !hinted {
            return false;
        }
        let now = networks();
        let mut held = self.nets.borrow_mut();
        if *held == now {
            return false;
        }
        *held = now;
        true
    }
}

/// The networks this machine can dial from, as text, sorted: the address
/// and its prefix, so a new lease on the same subnet counts too.
/// Loopback, link-local and unspecified addresses are left out
/// ([`crate::netmatch::LocalNet::usable`]) — nothing is reached over them,
/// and they are most of what churns on a laptop.
///
/// `ACS_NETWATCH_NETS` names a file to read them from instead (one CIDR per
/// line), so a test can say what the machine is on. It is read afresh every
/// time, as `getifaddrs` is.
fn networks() -> Vec<String> {
    let mut nets: Vec<String> = match std::env::var_os("ACS_NETWATCH_NETS") {
        Some(path) => std::fs::read_to_string(path)
            .unwrap_or_default()
            .split_whitespace()
            .map(String::from)
            .collect(),
        None => crate::netmatch::local_networks()
            .iter()
            .map(|n| format!("{}/{}", n.addr, n.prefix))
            .collect(),
    };
    nets.sort();
    nets.dedup();
    nets
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

    /// acs-6p8: a hint is a reason to look at the network, not a network
    /// change. Only the networks this machine could dial from say that, so
    /// the kernel may be as chatty as it likes for nothing.
    ///
    /// Both stand-ins are set here and nowhere else in this binary, and
    /// both are read while this test holds them: `ACS_NETWATCH_FIFO` by
    /// `NetWatch::new`, and `ACS_NETWATCH_NETS` by every `changed()` below.
    #[test]
    fn a_hint_is_a_change_only_when_the_networks_are_not_the_same() {
        let t = crate::testutil::TempDir::new();
        let fifo = t.path().join("net");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let nets = t.path().join("nets");
        let set = |text: &str| std::fs::write(&nets, text).unwrap();
        let hint = || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
                .unwrap()
        };
        set("192.168.1.5/24\nfd00::5/64\n");
        std::env::set_var("ACS_NETWATCH_FIFO", &fifo);
        std::env::set_var("ACS_NETWATCH_NETS", &nets);
        let w = NetWatch::new().unwrap();
        std::env::remove_var("ACS_NETWATCH_FIFO");
        // Nothing has been said at all.
        assert!(!w.changed());
        // The kernel speaks up while the machine is on the same networks —
        // an unrelated interface, a route churning, a probe. Twice, to say
        // that it is not merely the second one that is quiet.
        hint();
        assert!(!w.changed(), "a hint that changed nothing");
        hint();
        assert!(!w.changed(), "and the next one");
        // Order is not identity.
        set("fd00::5/64\n192.168.1.5/24\n");
        hint();
        assert!(!w.changed(), "the same networks in another order");
        // Wi-Fi switched: a new address, and the same prefix on a new
        // lease is a change too.
        set("10.0.0.5/24\nfd00::5/64\n");
        hint();
        assert!(w.changed());
        // Reported once: the kernel has more to say about the same change.
        hint();
        assert!(!w.changed());
        // An interface went away.
        set("fd00::5/64\n");
        hint();
        assert!(w.changed());
        std::env::remove_var("ACS_NETWATCH_NETS");
        // And this machine's own, read twice with nothing touched in
        // between: the same answer, or every hint would be a change again.
        // In the same test as the stand-in so the two cannot race over the
        // variable that tells them apart.
        let own = networks();
        assert_eq!(own, networks());
        let mut sorted = own.clone();
        sorted.sort();
        assert_eq!(
            own, sorted,
            "sorted: the order interfaces come in is not it"
        );
    }

    #[test]
    fn platform_watcher_opens() {
        assert!(NetWatch::platform().is_ok());
    }
}
