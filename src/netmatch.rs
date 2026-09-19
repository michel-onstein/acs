//! Which of an alias's hosts is on a network this machine is on (DESIGN
//! §7.3, `prefer_local_network`): the client's own networks from its
//! interfaces (`getifaddrs`: IPv4 netmask, IPv6 prefix length), matched
//! against the addresses each host name resolves to (`getaddrinfo`, A and
//! AAAA).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// A network this machine is on: one of its addresses and the prefix
/// length of that address's network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl LocalNet {
    pub fn new(addr: IpAddr, prefix: u8) -> LocalNet {
        LocalNet { addr, prefix }
    }

    /// Whether `ip` is on this network: the same family, and the same first
    /// `prefix` bits.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = mask32(self.prefix);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = mask128(self.prefix);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }

    /// Worth matching against: not loopback, link-local (which needs a
    /// scope to mean anything), unspecified, or a prefix of 0 (which would
    /// match every address).
    pub fn usable(&self) -> bool {
        if self.prefix == 0 {
            return false;
        }
        match self.addr {
            IpAddr::V4(a) => !(a.is_loopback() || a.is_link_local() || a.is_unspecified()),
            IpAddr::V6(a) => {
                !(a.is_loopback() || a.is_unspecified() || (a.segments()[0] & 0xffc0) == 0xfe80)
            }
        }
    }
}

/// The network in CIDR form: `192.168.1.0/24`, `fd00::/64`.
impl fmt::Display for LocalNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let net = match self.addr {
            IpAddr::V4(a) => IpAddr::V4(Ipv4Addr::from(u32::from(a) & mask32(self.prefix))),
            IpAddr::V6(a) => IpAddr::V6(Ipv6Addr::from(u128::from(a) & mask128(self.prefix))),
        };
        write!(f, "{net}/{}", self.prefix)
    }
}

fn mask32(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        p => u32::MAX << (32 - u32::from(p.min(32))),
    }
}

fn mask128(prefix: u8) -> u128 {
    match prefix {
        0 => 0,
        p => u128::MAX << (128 - u32::from(p.min(128))),
    }
}

/// The first of `nets` that one of `addrs` is on.
pub fn matching(nets: &[LocalNet], addrs: &[IpAddr]) -> Option<LocalNet> {
    nets.iter()
        .find(|n| addrs.iter().any(|a| n.contains(*a)))
        .copied()
}

/// Resolve a host name to its addresses within `deadline` (none if it does
/// not resolve in time).
pub type Resolve = dyn Fn(&str, Duration) -> Vec<IpAddr> + Send + Sync;

/// What matching needs: this machine's networks, and a resolver.
pub struct Network {
    pub local: Vec<LocalNet>,
    pub resolve: Arc<Resolve>,
}

/// The machine's own: its interfaces and the system resolver.
pub fn system() -> Network {
    Network {
        local: local_networks(),
        resolve: Arc::new(resolve),
    }
}

/// `host`'s addresses, A and AAAA: an address as written is itself; a name
/// is resolved by the system resolver, given up on at `deadline` (the
/// lookup runs on a thread of its own, left to finish).
pub fn resolve(host: &str, deadline: Duration) -> Vec<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    let (tx, rx) = mpsc::channel();
    let name = host.to_string();
    std::thread::spawn(move || {
        let addrs: Vec<IpAddr> = (name.as_str(), 0)
            .to_socket_addrs()
            .map(|a| a.map(|s| s.ip()).collect())
            .unwrap_or_default();
        let _ = tx.send(addrs);
    });
    rx.recv_timeout(deadline).unwrap_or_default()
}

/// The networks this machine is on: every up interface's address with its
/// prefix, the [`LocalNet::usable`] ones.
pub fn local_networks() -> Vec<LocalNet> {
    let mut nets = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list freed below; each node is
    // only read while the list lives.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return nets;
    }
    let mut p = head;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_flags & (libc::IFF_UP as libc::c_uint) == 0 {
            continue;
        }
        let Some(addr) = (unsafe { sockaddr_ip(ifa.ifa_addr) }) else {
            continue;
        };
        let Some(prefix) = (unsafe { mask_prefix(ifa.ifa_netmask, addr.is_ipv6()) }) else {
            continue;
        };
        let net = LocalNet::new(addr, prefix);
        if net.usable() {
            nets.push(net);
        }
    }
    unsafe { libc::freeifaddrs(head) };
    nets
}

/// The address of an `AF_INET` or `AF_INET6` socket address.
///
/// # Safety
/// `sa` is null or points to a socket address from `getifaddrs`.
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    match i32::from((*sa).sa_family) {
        libc::AF_INET => {
            let sin = &*(sa as *const libc::sockaddr_in);
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                sin.sin_addr.s_addr,
            ))))
        }
        libc::AF_INET6 => {
            let sin6 = &*(sa as *const libc::sockaddr_in6);
            Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// A netmask's prefix length, read as the family of the address it belongs
/// to: on the BSDs (macOS) a netmask's own family may be unset and its
/// length cut short after its last non-zero byte, the rest being zero.
///
/// # Safety
/// `sa` is null or points to a netmask from `getifaddrs`.
unsafe fn mask_prefix(sa: *const libc::sockaddr, v6: bool) -> Option<u8> {
    if sa.is_null() {
        return None;
    }
    let (offset, len) = if v6 {
        (std::mem::offset_of!(libc::sockaddr_in6, sin6_addr), 16)
    } else {
        (std::mem::offset_of!(libc::sockaddr_in, sin_addr), 4)
    };
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
    let have = usize::from((*sa).sa_len);
    #[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd")))]
    let have = offset + len;
    let bytes = sa as *const u8;
    let mut ones = 0u32;
    for i in 0..len {
        let at = offset + i;
        let b = if at < have { *bytes.add(at) } else { 0 };
        ones += b.count_ones();
    }
    Some(ones as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> LocalNet {
        let (a, p) = s.split_once('/').unwrap();
        LocalNet::new(a.parse().unwrap(), p.parse().unwrap())
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_address_is_on_a_network_by_its_prefix_in_either_family() {
        let home = net("192.168.1.5/24");
        assert!(home.contains(ip("192.168.1.20")));
        assert!(!home.contains(ip("192.168.2.20")));
        assert!(!home.contains(ip("fd00::20")), "another family");
        let odd = net("10.1.2.3/20");
        assert!(odd.contains(ip("10.1.15.255")));
        assert!(!odd.contains(ip("10.1.16.0")));
        let ula = net("fd12:3456:789a:1::5/64");
        assert!(ula.contains(ip("fd12:3456:789a:1::abcd")));
        assert!(!ula.contains(ip("fd12:3456:789a:2::abcd")));
        assert!(!ula.contains(ip("192.168.1.20")));
        assert!(net("2001:db8::1/128").contains(ip("2001:db8::1")));
        assert!(!net("2001:db8::1/128").contains(ip("2001:db8::2")));
        assert_eq!(home.to_string(), "192.168.1.0/24");
        assert_eq!(ula.to_string(), "fd12:3456:789a:1::/64");
    }

    #[test]
    fn loopback_link_local_and_a_zero_prefix_are_not_matched_on() {
        for s in [
            "127.0.0.1/8",
            "169.254.3.4/16",
            "::1/128",
            "fe80::1/64",
            "0.0.0.0/8",
            "10.0.0.1/0",
        ] {
            assert!(!net(s).usable(), "{s}");
        }
        for s in [
            "192.168.1.5/24",
            "10.8.0.2/32",
            "fd00::5/64",
            "2001:db8::5/64",
        ] {
            assert!(net(s).usable(), "{s}");
        }
    }

    #[test]
    fn the_first_network_an_address_is_on_is_the_match() {
        let nets = [net("192.168.1.5/24"), net("fd00::5/64")];
        assert_eq!(
            matching(&nets, &[ip("203.0.113.9"), ip("fd00::20")]),
            Some(nets[1])
        );
        assert_eq!(matching(&nets, &[ip("203.0.113.9")]), None);
        assert_eq!(matching(&nets, &[]), None);
    }

    #[test]
    fn an_address_resolves_to_itself_and_a_bad_name_to_nothing() {
        let d = Duration::from_secs(5);
        assert_eq!(resolve("192.0.2.7", d), [ip("192.0.2.7")]);
        assert_eq!(resolve("2001:db8::7", d), [ip("2001:db8::7")]);
        assert!(resolve("no-such-host.invalid", d).is_empty());
        let local = resolve("localhost", d);
        assert!(local.iter().all(|a| a.is_loopback()), "{local:?}");
    }

    #[test]
    fn this_machines_networks_are_usable_ones() {
        // Whatever the machine has: never loopback or link-local, and every
        // address on its own network.
        for n in local_networks() {
            eprintln!("local network: {n} (address {})", n.addr);
            assert!(n.usable(), "{n:?}");
            assert!(n.contains(n.addr), "{n:?}");
            let max = if n.addr.is_ipv6() { 128 } else { 32 };
            assert!(n.prefix <= max, "{n:?}");
        }
    }
}
