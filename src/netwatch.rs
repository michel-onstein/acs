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
//!
//! **The networks held move when the caller acts, not when it reads**
//! (acs-0n8). [`NetWatch::changed`] hands out a [`Change`]; the set it was
//! judged against is only replaced by [`Change::acted`]. A caller that
//! cannot act on the answer — mid-handshake, or inside the offline wait's
//! rate limit — drops it instead, and the next hint offers the same change
//! again rather than being answered "not a change" against a set that has
//! already moved. Nothing is queued by that: it is one comparison against
//! the networks of the moment however many hints went by, so a flapping
//! interface still costs one redial and not one per hint.
//!
//! **Under `-v` the watcher says what it made of every hint** (acs-4i2).
//! Silence is otherwise ambiguous in the one direction that costs: a
//! watcher that is working and a network that did not move look exactly
//! like a watcher that has stopped emitting — a macOS release changing
//! which messages accompany a roam, a VPN coming up without moving an
//! address [`crate::netmatch::LocalNet::usable`] accepts — and the feature
//! is load-bearing twice over (a wait cut short, acs-6p8; a dead link
//! found in 2 s instead of 10, acs-ft1). The lines read the decision and
//! never make it.

use std::cell::RefCell;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::sys;

pub struct NetWatch {
    fd: OwnedFd,
    kind: Kind,
    /// `-v`: say what each hint was judged to be ([`NetWatch::trace`]).
    verbose: bool,
    /// The networks as of the last change a caller **acted on**
    /// ([`Change::acted`], acs-0n8) — or as of the moment the watcher was
    /// made, so the first hint of an outage is judged like any other
    /// rather than taken on trust.
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
    /// `verbose` is the client's `-v` ([`NetWatch::trace`]).
    pub fn new(verbose: bool) -> Option<NetWatch> {
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
                verbose,
                nets: RefCell::new(networks()),
            });
        }
        Self::platform(verbose).ok()
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn platform(verbose: bool) -> io::Result<NetWatch> {
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
            verbose,
            nets: RefCell::new(networks()),
        })
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn platform(verbose: bool) -> io::Result<NetWatch> {
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
            verbose,
            nets: RefCell::new(networks()),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// One line on the terminal under `-v`, and nothing at all without it
    /// (acs-4i2).
    ///
    /// Strictly a reader of the decision: every call site below is reached
    /// after the answer is settled — [`Change`]'s included, which says
    /// only that the caller did not act — and nothing here touches the
    /// networks held, the rate limit the caller keeps
    /// ([`crate::reconnect`]'s `early_every`), or the value returned. That
    /// is the whole point — the trace exists to make a
    /// watcher that stopped emitting visible, not to change what a hint is
    /// worth.
    fn trace(&self, msg: &str) {
        if self.verbose {
            crate::client::note(msg);
        }
    }

    /// Drain the pending messages; a [`Change`] if the network this machine
    /// dials from is not the one it was on.
    ///
    /// Draining is unconditional — the descriptor has to be emptied for the
    /// poll it woke to sleep again — but a hint is only a reason to look
    /// (acs-6p8). What makes it a change is [`networks`] differing, and the
    /// answer is remembered **when the caller acts on it**
    /// ([`Change::acted`], acs-0n8), so a kernel that repeats itself is
    /// answered once and a change nobody could act on is offered again
    /// instead of being spent.
    pub fn changed(&self) -> Option<Change<'_>> {
        let mut buf = [0u8; 8192];
        let mut hinted = false;
        let mut bytes = 0usize;
        while let Ok(n) = sys::read(self.fd(), &mut buf) {
            if n == 0 {
                break;
            }
            bytes += n;
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
            // Only `Kind::Route` reaches this with anything read: the
            // kernel spoke and every message was route churn (ARP,
            // neighbour discovery). Worth a line — a macOS release that
            // changed which messages accompany a roam would show up here
            // and nowhere else.
            if bytes > 0 {
                self.trace(&format!(
                    "network: {bytes} bytes from the kernel, no address or interface message — not a change"
                ));
            }
            return None;
        }
        let now = networks();
        {
            let held = self.nets.borrow();
            if *held == now {
                self.trace(&format!(
                    "network hint: still on {} — not a change",
                    show(&now)
                ));
                return None;
            }
            self.trace(&format!(
                "network changed: {} → {}",
                show(held.as_slice()),
                show(&now)
            ));
        }
        Some(Change {
            watch: self,
            now,
            acted: false,
        })
    }
}

/// A change the caller has been told about and has not acted on yet
/// (acs-0n8).
///
/// The networks the watcher holds move on [`Change::acted`] and nowhere
/// else. A caller that cannot act on the answer drops this instead — the
/// handshake has its own deadline ([`crate::client`], acs-xo1 is acting on
/// it), and the offline wait allows one early redial per `ACS_EARLY_MS`
/// ([`crate::reconnect`]) — and the set stays where it was, so the next
/// hint reports the same change rather than answering "not a change"
/// against a set that has already moved. Before this the hint was consumed
/// either way and only a covering deadline recovered it.
///
/// Dropping it queues nothing. The change is not a message held in a box:
/// it is [`networks`] compared against the last set anybody acted on, made
/// afresh on each hint. However many a flapping interface sends, acting
/// once is one redial, and acs-6p8's contract is untouched — a hint that
/// moved no network is not a change at all.
#[must_use = "a change is only remembered once the caller says it acted on it"]
pub struct Change<'a> {
    watch: &'a NetWatch,
    /// The networks as of this change, held until [`Change::acted`].
    now: Vec<String>,
    acted: bool,
}

impl Change<'_> {
    /// The caller acted on the change — a redial, a ping — so these are the
    /// networks to judge the next hint against, and this change is not
    /// reported twice.
    pub fn acted(mut self) {
        *self.watch.nets.borrow_mut() = std::mem::take(&mut self.now);
        self.acted = true;
    }
}

impl Drop for Change<'_> {
    /// Under `-v`, say that the change is still standing: a reader who saw
    /// `network changed` and no redial would otherwise have to guess
    /// whether it was spent (acs-4i2's whole point). Nothing here decides
    /// anything — the set has simply not moved.
    fn drop(&mut self) {
        if !self.acted {
            self.watch
                .trace("network change not acted on — the next hint reports it again");
        }
    }
}

/// A set of networks as one line, for [`NetWatch::trace`].
fn show(nets: &[String]) -> String {
    if nets.is_empty() {
        return "no network of its own".to_string();
    }
    nets.join(" ")
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

    /// The two stand-ins are process-wide, so a test that wants them takes
    /// this first. They are set and removed in [`Standin`] and nowhere else
    /// in this binary, one test at a time: two of them at once would race
    /// over which machine the other one is on.
    static STANDIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The stand-ins acs-6p8 left, held for as long as one test needs them:
    /// `ACS_NETWATCH_FIFO` for the kernel's hint (read once, by
    /// `NetWatch::new`) and `ACS_NETWATCH_NETS` for the machine's networks
    /// (read afresh by every `changed()`).
    struct Standin {
        dir: crate::testutil::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Standin {
        /// A watcher on a machine that is on `nets`, with a FIFO of this
        /// test's own for the hints.
        fn watching(nets: &str) -> (Standin, NetWatch) {
            // A test that fails while holding it poisons nothing that
            // matters: the next one sets both variables itself.
            let lock = STANDIN.lock().unwrap_or_else(|e| e.into_inner());
            let s = Standin {
                dir: crate::testutil::TempDir::new(),
                _lock: lock,
            };
            let c = std::ffi::CString::new(s.fifo().to_str().unwrap()).unwrap();
            // SAFETY: a path in this test's own temporary directory.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            s.set(nets);
            std::env::set_var("ACS_NETWATCH_FIFO", s.fifo());
            std::env::set_var("ACS_NETWATCH_NETS", s.dir.path().join("nets"));
            let w = NetWatch::new(false).unwrap();
            // Spent: the watcher has its descriptor, and nothing else in
            // this process should open the FIFO.
            std::env::remove_var("ACS_NETWATCH_FIFO");
            (s, w)
        }

        fn fifo(&self) -> std::path::PathBuf {
            self.dir.path().join("net")
        }

        /// The machine is on these networks from now on.
        fn set(&self, nets: &str) {
            std::fs::write(self.dir.path().join("nets"), nets).unwrap();
        }

        /// The kernel said something about the network.
        fn hint(&self) {
            std::fs::OpenOptions::new()
                .write(true)
                .open(self.fifo())
                .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
                .unwrap();
        }

        /// Give the machine its real networks back, for the assertions that
        /// are about `getifaddrs` itself.
        fn release(&self) {
            std::env::remove_var("ACS_NETWATCH_NETS");
        }
    }

    impl Drop for Standin {
        fn drop(&mut self) {
            self.release();
        }
    }

    /// acs-6p8: a hint is a reason to look at the network, not a network
    /// change. Only the networks this machine could dial from say that, so
    /// the kernel may be as chatty as it likes for nothing.
    #[test]
    fn a_hint_is_a_change_only_when_the_networks_are_not_the_same() {
        let (s, w) = Standin::watching("192.168.1.5/24\nfd00::5/64\n");
        // Acting on whatever a hint turns out to be, which is what every
        // caller in this binary does when it can (acs-0n8).
        let changed = || match w.changed() {
            Some(c) => {
                c.acted();
                true
            }
            None => false,
        };
        // Nothing has been said at all.
        assert!(!changed());
        // The kernel speaks up while the machine is on the same networks —
        // an unrelated interface, a route churning, a probe. Twice, to say
        // that it is not merely the second one that is quiet.
        s.hint();
        assert!(!changed(), "a hint that changed nothing");
        s.hint();
        assert!(!changed(), "and the next one");
        // Order is not identity.
        s.set("fd00::5/64\n192.168.1.5/24\n");
        s.hint();
        assert!(!changed(), "the same networks in another order");
        // Wi-Fi switched: a new address, and the same prefix on a new
        // lease is a change too.
        s.set("10.0.0.5/24\nfd00::5/64\n");
        s.hint();
        assert!(changed());
        // Reported once — to a caller that acted on it: the kernel has
        // more to say about the same change.
        s.hint();
        assert!(!changed());
        // An interface went away.
        s.set("fd00::5/64\n");
        s.hint();
        assert!(changed());
        s.release();
        // And this machine's own, read twice with nothing touched in
        // between: the same answer, or every hint would be a change again.
        // Still holding `STANDIN`, so nothing else in this binary can set
        // the variable that tells the two apart while this reads them.
        let own = networks();
        assert_eq!(own, networks());
        let mut sorted = own.clone();
        sorted.sort();
        assert_eq!(
            own, sorted,
            "sorted: the order interfaces come in is not it"
        );
    }

    /// Regression (acs-0n8): a change the caller could not act on is not
    /// spent — the next hint reports it again — and a flap that produces
    /// several of them is still one change when somebody finally acts.
    ///
    /// Before this, `changed()` moved the networks held whether or not the
    /// answer was used, so the second hint of the same change compared
    /// against a set that had already moved and said "not a change". The
    /// change was not deferred, it was gone, and only a covering deadline
    /// (the dial timeout, the backoff) recovered it.
    ///
    /// The same stand-ins as the test above, one test at a time.
    #[test]
    fn a_change_nobody_acted_on_is_offered_again() {
        let (s, w) = Standin::watching("192.168.1.5/24\n");
        // Wi-Fi switched while the caller could do nothing with it — it was
        // mid-handshake, or its last early redial was too recent. The
        // change is read and dropped.
        s.set("10.0.0.5/24\n");
        s.hint();
        assert!(w.changed().is_some(), "the networks moved");
        // The kernel speaks again about the very same state of the world.
        // This is the drop path: the answer has to be the change again.
        s.hint();
        assert!(
            w.changed().is_some(),
            "the change was spent by a caller that could not use it"
        );

        // A flapping interface: three more changes in a row, none of them
        // acted on. Nothing queues — what is offered is always the
        // networks of the moment against the last set acted on.
        for n in ["10.0.0.6/24\n", "10.0.0.7/24\n", "10.0.0.8/24\n"] {
            s.set(n);
            s.hint();
            assert!(w.changed().is_some(), "{n} is a change from 192.168.1.5");
        }
        // Somebody acts at last: one redial's worth, not four.
        s.set("10.0.0.9/24\n");
        s.hint();
        w.changed().expect("still a change").acted();
        s.hint();
        assert!(
            w.changed().is_none(),
            "the flap left changes behind to be reported again"
        );
    }

    #[test]
    fn platform_watcher_opens() {
        assert!(NetWatch::platform(false).is_ok());
    }

    /// The trace's own text, so the line a reader looks for is pinned
    /// somewhere a change to it has to be deliberate (acs-4i2).
    #[test]
    fn a_set_of_networks_reads_as_a_line() {
        assert_eq!(show(&[]), "no network of its own");
        assert_eq!(show(&["192.168.1.5/24".to_string()]), "192.168.1.5/24");
        assert_eq!(
            show(&["192.168.1.5/24".to_string(), "fd00::5/64".to_string()]),
            "192.168.1.5/24 fd00::5/64"
        );
    }
}
