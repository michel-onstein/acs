//! Port knocking for hosts behind knockd (KNOCK_DESIGN §4): the sequence
//! and its timing ([`Knock`], [`Knock::parse`]), the sender ([`send`]) and
//! the 2 s reuse table that keeps a probe-then-dial from knocking twice
//! (§3.4).
//!
//! A TCP knock is a non-blocking `connect()` closed at once, a UDP knock an
//! empty datagram: ordinary sockets, no privileges. Closing the TCP socket
//! at once is load-bearing — one left open retransmits its SYN a second
//! later, and knockd takes that for an out-of-order knock and starts the
//! sequence over (§4.1).

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::format_timeout;
use crate::ssh::{AddressFamily, Route};

/// How long one knock waits before the next, when nothing sets it.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(50);
/// How long the last knock waits before ssh is spawned, when nothing sets it.
pub const DEFAULT_WAIT: Duration = Duration::from_millis(200);
/// The longest `delay` accepted.
pub const MAX_DELAY: Duration = Duration::from_secs(5);
/// The longest `wait` accepted.
pub const MAX_WAIT: Duration = Duration::from_secs(10);
/// knockd's default `seq_timeout`: delays adding up to more could never
/// complete a sequence.
pub const MAX_SEQUENCE_TIME: Duration = Duration::from_secs(15);
/// The most knocks a sequence may have.
pub const MAX_KNOCKS: usize = 32;
/// A knock to the same address with the same sequence within this long is
/// reused rather than sent again (§3.4).
pub const REUSE: Duration = Duration::from_secs(2);

/// The protocol of one knock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

/// A host's knock: the ports to hit in order, the delay between them, the
/// wait after the last, and the host to knock when it is not the one ssh
/// connects to (KNOCK_DESIGN §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knock {
    pub sequence: Vec<(u16, Proto)>,
    pub delay: Duration,
    pub wait: Duration,
    pub host: Option<String>,
}

impl Knock {
    /// A sequence as knockd writes one — `7000,8000:udp,9000` — with the
    /// default timing and no host of its own.
    pub fn parse(text: &str) -> Result<Knock, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err(format!(
                "expected a knock sequence such as 7000,8000:udp,9000, found '{text}'"
            ));
        }
        let mut sequence = Vec::new();
        for (i, element) in t.split(',').enumerate() {
            let element = element.trim();
            if element.is_empty() {
                return Err(format!("'{t}': knock {} is empty", i + 1));
            }
            let (port, proto) = match element.split_once(':') {
                Some((p, proto)) => (p.trim_end(), proto.trim_start()),
                None => (element, "tcp"),
            };
            let proto = match proto.to_ascii_lowercase().as_str() {
                "tcp" => Proto::Tcp,
                "udp" => Proto::Udp,
                _ => return Err(format!("'{t}': '{proto}' is not tcp or udp")),
            };
            let port = match port.parse::<u16>() {
                Ok(p) if p > 0 && port.bytes().all(|b| b.is_ascii_digit()) => p,
                _ => return Err(format!("'{t}': '{port}' is not a port from 1 to 65535")),
            };
            sequence.push((port, proto));
        }
        Knock::new(sequence, DEFAULT_DELAY, DEFAULT_WAIT, None)
    }

    /// A knock from its parts, refused when the sequence is empty or longer
    /// than [`MAX_KNOCKS`], or the delays add up to more than knockd's
    /// `seq_timeout` (§4.3). The delay and the wait come bounded from
    /// [`parse_delay`] and [`parse_wait`], whose messages name the value as
    /// written, so they are not checked twice (acs-fode).
    pub fn new(
        sequence: Vec<(u16, Proto)>,
        delay: Duration,
        wait: Duration,
        host: Option<String>,
    ) -> Result<Knock, String> {
        let knock = Knock {
            sequence,
            delay,
            wait,
            host,
        };
        knock.check().map(|()| knock)
    }

    fn check(&self) -> Result<(), String> {
        let n = self.sequence.len();
        if n == 0 {
            return Err("a knock sequence needs at least one port".into());
        }
        let total = self.delay * (n as u32 - 1);
        let why = if n > MAX_KNOCKS {
            format!("{n} knocks, at most {MAX_KNOCKS}")
        } else if total > MAX_SEQUENCE_TIME {
            format!(
                "{n} knocks {} apart take {}, more than knockd's {} seq_timeout",
                format_timeout(self.delay),
                format_timeout(total),
                format_timeout(MAX_SEQUENCE_TIME)
            )
        } else {
            return Ok(());
        };
        Err(format!("'{self}': {why}"))
    }

    /// Set the timing key `key` — `delay`, `wait` or `host` — from `value`,
    /// for the file's mapping and the command line's `/key=value` alike
    /// (§6): `Ok(false)` for a key it does not know, an error naming the
    /// key for a bad value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool, String> {
        match key {
            "delay" => parse_delay(value).map(|d| self.delay = d),
            "wait" => parse_wait(value).map(|w| self.wait = w),
            "host" => parse_host(value).map(|h| self.host = Some(h)),
            _ => return Ok(false),
        }
        .map(|()| true)
        .map_err(|e| format!("{key}: {e}"))
    }
}

/// The word that drops a configured knock (§6): `knock: none` in the file,
/// `--knock none` and `ACS_KNOCK=none`.
pub const NO_KNOCK: &str = "none";

/// The keys of a knock written as a mapping in the file (§6.1), as
/// messages list them: `sequence`, then those [`Knock::set`] takes.
pub const KNOCK_KEYS: &str = "sequence, delay, wait, host";

/// A knock's `delay` (§4.3): a duration as `reachability_timeout` is
/// written, from 0 to [`MAX_DELAY`].
pub fn parse_delay(text: &str) -> Result<Duration, String> {
    crate::config::parse_duration(text, Duration::ZERO, MAX_DELAY)
}

/// A knock's `wait` (§4.3): from 0 to [`MAX_WAIT`].
pub fn parse_wait(text: &str) -> Result<Duration, String> {
    crate::config::parse_duration(text, Duration::ZERO, MAX_WAIT)
}

/// A knock's `host` (§5): a host name or address, as an alias entry's
/// `host` is — no login name, spaces or leading `-`.
pub fn parse_host(text: &str) -> Result<String, String> {
    let h = text.trim();
    if h.is_empty()
        || h.starts_with('-')
        || h.contains('@')
        || h.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "bad knock host '{text}' (a host name or address, no '@')"
        ));
    }
    Ok(h.to_string())
}

/// A knock as the command line's `--knock` and `ACS_KNOCK` give it (§6.2):
/// the sequence of [`Knock::parse`], then optionally `/delay=…`, `/wait=…`
/// and `/host=…`, in any order. `none` is `Ok(None)`: drop the configured
/// knock for this run.
pub fn parse_spec(text: &str) -> Result<Option<Knock>, String> {
    let t = text.trim();
    if t.eq_ignore_ascii_case(NO_KNOCK) {
        return Ok(None);
    }
    let mut parts = t.split('/');
    let mut knock = Knock::parse(parts.next().unwrap_or_default())?;
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!(
                "'/{part}': expected /delay=, /wait= or /host= after the sequence"
            ));
        };
        let key = key.trim();
        if !knock.set(key, value)? {
            return Err(format!(
                "unknown knock option '{key}' (known: delay, wait, host)"
            ));
        }
    }
    knock.check().map(|()| Some(knock))
}

/// The sequence in knockd's syntax, `:udp` written only where it applies.
impl fmt::Display for Knock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (port, proto)) in self.sequence.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{port}")?;
            if *proto == Proto::Udp {
                f.write_str(":udp")?;
            }
        }
        Ok(())
    }
}

/// What [`send`] did: the addresses it knocked, and those it left alone
/// because they were knocked with the same sequence less than [`REUSE`]
/// ago, with how long ago the latest of those was. An address dropped as
/// unreachable is in neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub knocked: Vec<IpAddr>,
    pub reused: Vec<IpAddr>,
    pub ago: Option<Duration>,
}

/// The socket calls a knock makes, and its sleeps — behind a trait so the
/// tests can record them instead.
pub trait Net {
    fn tcp(&mut self, to: SocketAddr) -> io::Result<()>;
    fn udp(&mut self, to: SocketAddr) -> io::Result<()>;
    fn sleep(&mut self, d: Duration);
}

/// The real sockets.
pub struct Sockets;

impl Net for Sockets {
    fn tcp(&mut self, to: SocketAddr) -> io::Result<()> {
        knock_one(to, Proto::Tcp)
    }

    fn udp(&mut self, to: SocketAddr) -> io::Result<()> {
        knock_one(to, Proto::Udp)
    }

    fn sleep(&mut self, d: Duration) {
        thread::sleep(d);
    }
}

/// One knock from a socket closed before this returns: for TCP a
/// non-blocking connect, for UDP an empty datagram. The same socket calls
/// for both, rather than std's `UdpSocket` beside them (acs-fode).
fn knock_one(to: SocketAddr, proto: Proto) -> io::Result<()> {
    let family = match to {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    let kind = match proto {
        Proto::Tcp => libc::SOCK_STREAM,
        Proto::Udp => libc::SOCK_DGRAM,
    };
    let raw = crate::sys::cvt(unsafe { libc::socket(family, kind, 0) })?;
    // Owned from here on, so every path out closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    crate::sys::set_cloexec(raw)?;
    crate::sys::set_nonblocking(raw, true)?;
    let (storage, len) = sockaddr(to);
    let at = &storage as *const libc::sockaddr_storage as *const libc::sockaddr;
    let r = unsafe {
        match proto {
            Proto::Tcp => libc::connect(raw, at, len) as isize,
            Proto::Udp => libc::sendto(raw, std::ptr::null(), 0, 0, at, len),
        }
    };
    let err = (r < 0).then(io::Error::last_os_error);
    // Closed at once: in SYN_SENT this discards the socket before it can
    // retransmit (§4.1); an ICMP unreachable a datagram brings back is
    // never read.
    drop(fd);
    match err.as_ref().and_then(io::Error::raw_os_error) {
        // Sent, and still on its way; or sent and refused.
        Some(libc::EINPROGRESS) | Some(libc::ECONNREFUSED) => Ok(()),
        Some(_) => Err(err.unwrap()),
        // Completed at once: something listens there, or the datagram went.
        None => Ok(()),
    }
}

/// `to` as the kernel takes it.
fn sockaddr(to: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is valid, and both shapes fit it.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match to {
        SocketAddr::V4(a) => {
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from(*a.ip()).to_be();
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_scope_id = a.scope_id();
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        storage.ss_len = len as u8;
    }
    (storage, len as libc::socklen_t)
}

/// An error that says the packet never left: that address is dropped from
/// the knock rather than failing it (§4.1).
fn unreachable(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENETUNREACH | libc::EHOSTUNREACH | libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
    )
}

/// One knock remembered: where, with which sequence, and when it ended.
type Entry = (IpAddr, Vec<(u16, Proto)>, Instant);

/// The last knock to each address with each sequence, process-wide
/// (§3.4).
#[derive(Debug, Default)]
pub struct Recent {
    entries: Vec<Entry>,
}

impl Recent {
    pub const fn new() -> Recent {
        Recent {
            entries: Vec::new(),
        }
    }

    /// Whether `addr` was knocked with `sequence` less than [`REUSE`]
    /// before `now`.
    pub fn fresh(&self, addr: IpAddr, sequence: &[(u16, Proto)], now: Instant) -> bool {
        self.age(addr, sequence, now).is_some()
    }

    /// How long before `now` `addr` was knocked with `sequence`, when that
    /// is less than [`REUSE`].
    pub fn age(&self, addr: IpAddr, sequence: &[(u16, Proto)], now: Instant) -> Option<Duration> {
        self.entries
            .iter()
            .filter(|(a, s, _)| *a == addr && s == sequence)
            .map(|(_, _, at)| now.saturating_duration_since(*at))
            .find(|age| *age < REUSE)
    }

    /// Remember a knock to `addr` with `sequence` at `at`, forgetting those
    /// too old to be reused.
    pub fn note(&mut self, addr: IpAddr, sequence: &[(u16, Proto)], at: Instant) {
        self.entries.retain(|(a, s, t)| {
            at.saturating_duration_since(*t) < REUSE && !(*a == addr && s == sequence)
        });
        self.entries.push((addr, sequence.to_vec(), at));
    }
}

static RECENT: Mutex<Recent> = Mutex::new(Recent::new());

/// Knock every address in `addrs` with `knock`: each knock goes to all of
/// them in turn, then the delay before the next; after the last, the wait.
/// An address knocked with the same sequence in the last [`REUSE`] is left
/// alone, and when all are, nothing is sent and nothing waited for.
pub fn send(addrs: &[IpAddr], knock: &Knock) -> Result<Outcome, String> {
    send_with(&mut Sockets, &RECENT, addrs, knock)
}

/// [`send`] over `net`, with `recent` for its reuse table.
pub fn send_with(
    net: &mut dyn Net,
    recent: &Mutex<Recent>,
    addrs: &[IpAddr],
    knock: &Knock,
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let mut live = Vec::new();
    {
        let table = recent.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        for &a in addrs {
            if let Some(age) = table.age(a, &knock.sequence, now) {
                outcome.reused.push(a);
                outcome.ago = Some(outcome.ago.map_or(age, |o| o.min(age)));
            } else if !live.contains(&a) {
                live.push(a);
            }
        }
    }
    if live.is_empty() {
        if outcome.reused.is_empty() {
            return Err("no address to knock".into());
        }
        return Ok(outcome);
    }
    let mut last_drop = String::new();
    for (i, &(port, proto)) in knock.sequence.iter().enumerate() {
        if i > 0 && !knock.delay.is_zero() {
            net.sleep(knock.delay);
        }
        let mut kept = Vec::with_capacity(live.len());
        for &a in &live {
            let to = SocketAddr::new(a, port);
            let sent = match proto {
                Proto::Tcp => net.tcp(to),
                Proto::Udp => net.udp(to),
            };
            match sent {
                Ok(()) => kept.push(a),
                Err(e) => {
                    // The address alone: the port is the sequence, never told.
                    last_drop = format!("{a}: {e}");
                    if !unreachable(&e) {
                        return Err(last_drop);
                    }
                }
            }
        }
        live = kept;
        if live.is_empty() {
            // Only reached through a drop, so `last_drop` is set.
            return Err(format!("no address can be reached ({last_drop})"));
        }
    }
    let done = Instant::now();
    {
        let mut table = recent.lock().unwrap_or_else(|e| e.into_inner());
        for &a in &live {
            table.note(a, &knock.sequence, done);
        }
    }
    if !knock.wait.is_zero() {
        net.sleep(knock.wait);
    }
    outcome.knocked = live;
    Ok(outcome)
}

/// Why a dial cannot knock (§3.2, §5, §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A configuration that cannot work — a knock on a proxied dial (§5):
    /// the error code, and a redial does not try it again.
    Config(String),
    /// Where to knock is not known, or nothing could be sent to it: as an
    /// unreachable host, which a redial tries again.
    Unreachable(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Config(m) | Refusal::Unreachable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refusal {}

impl Knock {
    /// How long sending it takes: the delays between the knocks, then the
    /// wait (§4.3).
    pub fn duration(&self) -> Duration {
        self.delay * (self.sequence.len() as u32).saturating_sub(1) + self.wait
    }
}

/// The host a dial to `dest` knocks (§3.2, §5): `knock.host` when it names
/// one, whatever the route; else the host `ssh -G` says ssh connects to —
/// refused when that is through a proxy, and when `ssh -G` could not say.
pub fn target<'a>(
    knock: &'a Knock,
    dest: &str,
    route: Option<&'a Route>,
) -> Result<&'a str, Refusal> {
    if let Some(h) = &knock.host {
        return Ok(h);
    }
    match route {
        Some(r) if r.proxied => Err(Refusal::Config(format!(
            "knock: {dest} is reached through {}, so a knock from here would open it to the wrong address; \
             set knock.host to knock the jump host, or knock from the jump host's ProxyCommand",
            r.proxy.as_deref().unwrap_or("a proxy")
        ))),
        Some(Route {
            hostname: Some(h), ..
        }) => Ok(h),
        _ => Err(Refusal::Unreachable(format!(
            "knock: cannot tell where ssh connects for {dest} (ssh -G failed)"
        ))),
    }
}

/// The addresses of `addrs` that ssh may connect to with `family` (§3.3).
pub fn by_family(mut addrs: Vec<IpAddr>, family: AddressFamily) -> Vec<IpAddr> {
    addrs.retain(|a| match family {
        AddressFamily::Any => true,
        AddressFamily::Inet => a.is_ipv4(),
        AddressFamily::Inet6 => a.is_ipv6(),
    });
    addrs
}

/// The `-v` lines for a knock to `target` (§7): what was knocked and how,
/// and what was knocked so recently that it was not knocked again. They
/// name the host, its addresses and the count, **never the ports**. Each
/// goes to `log` as it is made.
pub fn report(target: &str, knock: &Knock, o: &Outcome, log: &mut dyn FnMut(String)) {
    use fmt::Write;
    if !o.knocked.is_empty() {
        // One string written into, not a `format!` per part (acs-fode).
        let n = knock.sequence.len();
        let mut l = format!("knocking {target} (");
        for (i, a) in o.knocked.iter().enumerate() {
            let _ = write!(l, "{}{a}", if i > 0 { ", " } else { "" });
        }
        let _ = write!(l, "): {n} knock{}", if n == 1 { "" } else { "s" });
        if n > 1 {
            let _ = write!(l, ", {} ms apart", knock.delay.as_millis() as u64);
        }
        let _ = write!(l, ", then {} ms", knock.wait.as_millis() as u64);
        log(l);
    }
    if let Some(ago) = o.ago {
        // Tenths, rounded, as `{:.1}` of the seconds would.
        let t = (ago.as_millis() as u64 + 50) / 100;
        log(format!(
            "{target} was knocked {}.{} s ago — not knocking again",
            t / 10,
            t % 10
        ));
    }
}

/// Knock where a dial to `dest` will connect (§3): the [`target`], its
/// addresses looked up within `lookup` and kept to the route's address
/// family, then [`send`]. `log` is given the [`report`] lines. The host
/// knocked, for the hint of §7.
pub fn before_dial(
    knock: &Knock,
    dest: &str,
    route: Option<&Route>,
    lookup: Duration,
    log: &mut dyn FnMut(String),
) -> Result<String, Refusal> {
    let target = self::target(knock, dest, route)?;
    let family = route.map(|r| r.addressfamily).unwrap_or_default();
    let addrs = by_family(crate::netmatch::resolve(target, lookup), family);
    if addrs.is_empty() {
        return Err(Refusal::Unreachable(format!(
            "knock: no address for {target}"
        )));
    }
    let outcome =
        send(&addrs, knock).map_err(|e| Refusal::Unreachable(format!("knock: {target}: {e}")))?;
    report(target, knock, &outcome, log);
    Ok(target.to_string())
}

/// The line §7 adds when ssh could not connect to a host it knocked: the
/// question a user would otherwise have no way to ask.
pub fn hint(target: &str) -> String {
    format!(
        "{target} was knocked before this dial; if it is up, the sequence or its timing may not match knockd's (see knockd's log)"
    )
}

/// Whether ssh's stderr says it could not connect at all — a timeout or a
/// refusal — which is how a knock that did not open the port ends.
/// Bytes, not text: ssh's stderr is read as it comes (acs-fode).
pub fn connect_failed(stderr: &[u8]) -> bool {
    [
        &b"Connection timed out"[..],
        b"Operation timed out",
        b"Connection refused",
    ]
    .iter()
    .any(|m| stderr.windows(m.len()).any(|w| w == *m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream, UdpSocket};

    const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

    fn seq(k: &Knock) -> Vec<(u16, Proto)> {
        k.sequence.clone()
    }

    #[test]
    fn the_command_line_spec_takes_timing_and_a_host() {
        let k = parse_spec("7000,8000:udp,9000").unwrap().unwrap();
        assert_eq!(k, Knock::parse("7000,8000:udp,9000").unwrap());
        let k = parse_spec("7000,8000,9000/delay=100ms/wait=0.5s")
            .unwrap()
            .unwrap();
        assert_eq!(k.delay, Duration::from_millis(100));
        assert_eq!(k.wait, Duration::from_millis(500));
        assert_eq!(k.host, None);
        let k = parse_spec("7000/host=bastion.example.com/delay=0")
            .unwrap()
            .unwrap();
        assert_eq!(k.host.as_deref(), Some("bastion.example.com"));
        assert_eq!(k.delay, Duration::ZERO);
        assert_eq!(k.wait, DEFAULT_WAIT);
        for none in ["none", "NONE", " none "] {
            assert_eq!(parse_spec(none), Ok(None), "{none}");
        }
    }

    #[test]
    fn a_bad_command_line_spec_says_why() {
        for (spec, want) in [
            ("", "expected a knock sequence"),
            ("7000,,9000", "knock 2 is empty"),
            ("7000/delay", "expected /delay=, /wait= or /host="),
            (
                "7000/pause=1s",
                "unknown knock option 'pause' (known: delay, wait, host)",
            ),
            ("7000/delay=6s", "delay: '6s' is too long: at most 5s"),
            ("7000/wait=11s", "wait: '11s' is too long: at most 10s"),
            ("7000/wait=soon", "wait: expected a duration"),
            ("7000/host=", "bad knock host ''"),
            ("7000/host=me@b", "bad knock host 'me@b'"),
            ("7000/host=-oProxy", "bad knock host '-oProxy'"),
            (
                "7000,8000,9000,9001,9002/delay=5s",
                "more than knockd's 15s seq_timeout",
            ),
        ] {
            let e = parse_spec(spec).unwrap_err();
            assert!(e.contains(want), "{spec}: {e}");
        }
    }

    #[test]
    fn parses_knockd_sequences() {
        let k = Knock::parse("7000,8000:udp,9000").unwrap();
        assert_eq!(
            seq(&k),
            [(7000, Proto::Tcp), (8000, Proto::Udp), (9000, Proto::Tcp)]
        );
        assert_eq!(k.delay, DEFAULT_DELAY);
        assert_eq!(k.wait, DEFAULT_WAIT);
        assert_eq!(k.host, None);
        let spaced = Knock::parse(" 7000 , 8000 : UDP,9000:tcp ,7000").unwrap();
        assert_eq!(
            seq(&spaced),
            [
                (7000, Proto::Tcp),
                (8000, Proto::Udp),
                (9000, Proto::Tcp),
                (7000, Proto::Tcp)
            ]
        );
        assert_eq!(Knock::parse("1").unwrap().sequence, [(1, Proto::Tcp)]);
        assert_eq!(
            Knock::parse("65535:udp").unwrap().sequence,
            [(65535, Proto::Udp)]
        );
    }

    #[test]
    fn writes_back_what_it_reads() {
        let k = Knock::parse("7000, 8000:UDP ,9000:tcp").unwrap();
        assert_eq!(k.to_string(), "7000,8000:udp,9000");
        assert_eq!(Knock::parse(&k.to_string()).unwrap(), k);
    }

    #[test]
    fn refuses_bad_sequences_saying_why() {
        let cases = [
            (
                "",
                "expected a knock sequence such as 7000,8000:udp,9000, found ''",
            ),
            (
                "  ",
                "expected a knock sequence such as 7000,8000:udp,9000, found '  '",
            ),
            ("7000,,9000", "'7000,,9000': knock 2 is empty"),
            ("7000,", "'7000,': knock 2 is empty"),
            ("7000:icmp", "'7000:icmp': 'icmp' is not tcp or udp"),
            ("7000:", "'7000:': '' is not tcp or udp"),
            ("0", "'0': '0' is not a port from 1 to 65535"),
            ("65536", "'65536': '65536' is not a port from 1 to 65535"),
            ("+80", "'+80': '+80' is not a port from 1 to 65535"),
            ("ssh", "'ssh': 'ssh' is not a port from 1 to 65535"),
            (":udp", "':udp': '' is not a port from 1 to 65535"),
        ];
        for (text, message) in cases {
            assert_eq!(Knock::parse(text).unwrap_err(), message, "{text:?}");
        }
    }

    #[test]
    fn a_sequence_has_at_most_32_knocks() {
        let ports = |n: u16| (1..=n).map(|p| p.to_string()).collect::<Vec<_>>().join(",");
        assert_eq!(Knock::parse(&ports(32)).unwrap().sequence.len(), 32);
        let err = Knock::parse(&ports(33)).unwrap_err();
        assert!(err.ends_with("': 33 knocks, at most 32"), "{err}");
        assert_eq!(
            Knock::new(vec![], DEFAULT_DELAY, DEFAULT_WAIT, None).unwrap_err(),
            "a knock sequence needs at least one port"
        );
    }

    #[test]
    fn timing_is_bounded() {
        let three = Knock::parse("1,2,3").unwrap().sequence;
        let new = |d: u64, w: u64| {
            Knock::new(
                three.clone(),
                Duration::from_millis(d),
                Duration::from_millis(w),
                None,
            )
        };
        assert!(new(0, 0).is_ok());
        assert!(new(5000, 10_000).is_ok());
        // The delay and the wait are bounded where they are read.
        let mut k = new(0, 0).unwrap();
        assert_eq!(k.set("delay", "5s"), Ok(true));
        assert_eq!(
            k.set("delay", "5001ms").unwrap_err(),
            "delay: '5001ms' is too long: at most 5s"
        );
        assert_eq!(
            k.set("wait", "10001ms").unwrap_err(),
            "wait: '10001ms' is too long: at most 10s"
        );
        assert_eq!(k.set("pause", "1s"), Ok(false));
        assert_eq!((k.delay, k.wait), (MAX_DELAY, Duration::ZERO));
        // Sixteen knocks a second apart take 15 s: the limit, accepted.
        let ports = |n: u16| (1..=n).map(|p| (p, Proto::Tcp)).collect::<Vec<_>>();
        let second = Duration::from_secs(1);
        assert!(Knock::new(ports(16), second, DEFAULT_WAIT, None).is_ok());
        assert_eq!(
            Knock::new(ports(17), second, DEFAULT_WAIT, None).unwrap_err(),
            "'1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17': 17 knocks 1s apart take 16s, \
             more than knockd's 15s seq_timeout"
        );
        // One knock has no delay to add up, whatever the delay.
        assert!(Knock::new(ports(1), MAX_DELAY, DEFAULT_WAIT, None).is_ok());
    }

    /// What a knock would have done, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Tcp(SocketAddr),
        Udp(SocketAddr),
        Sleep(Duration),
    }

    #[derive(Default)]
    struct Recorder {
        events: Vec<Event>,
        /// Addresses that answer every send with this errno.
        fail: Vec<(IpAddr, i32)>,
    }

    impl Recorder {
        fn result(&self, to: SocketAddr) -> io::Result<()> {
            match self.fail.iter().find(|(a, _)| *a == to.ip()) {
                Some(&(_, errno)) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }
    }

    impl Net for Recorder {
        fn tcp(&mut self, to: SocketAddr) -> io::Result<()> {
            self.events.push(Event::Tcp(to));
            self.result(to)
        }
        fn udp(&mut self, to: SocketAddr) -> io::Result<()> {
            self.events.push(Event::Udp(to));
            self.result(to)
        }
        fn sleep(&mut self, d: Duration) {
            self.events.push(Event::Sleep(d));
        }
    }

    fn knock(text: &str) -> Knock {
        Knock::parse(text).unwrap()
    }

    #[test]
    fn knocks_every_address_per_knock_with_the_delay_between() {
        let mut net = Recorder::default();
        let table = Mutex::new(Recent::new());
        let out = send_with(&mut net, &table, &[V4, V6], &knock("7000,8000:udp,9000")).unwrap();
        assert_eq!(out.knocked, [V4, V6]);
        assert!(out.reused.is_empty());
        let s = |a, p| SocketAddr::new(a, p);
        use Event::*;
        assert_eq!(
            net.events,
            [
                Tcp(s(V4, 7000)),
                Tcp(s(V6, 7000)),
                Sleep(DEFAULT_DELAY),
                Udp(s(V4, 8000)),
                Udp(s(V6, 8000)),
                Sleep(DEFAULT_DELAY),
                Tcp(s(V4, 9000)),
                Tcp(s(V6, 9000)),
                Sleep(DEFAULT_WAIT),
            ]
        );
    }

    #[test]
    fn zero_delay_and_wait_do_not_sleep() {
        let mut net = Recorder::default();
        let table = Mutex::new(Recent::new());
        let k = Knock::new(knock("1,2").sequence, Duration::ZERO, Duration::ZERO, None).unwrap();
        send_with(&mut net, &table, &[V4, V4], &k).unwrap();
        let s = |p| Event::Tcp(SocketAddr::new(V4, p));
        // A repeated address is knocked once.
        assert_eq!(net.events, [s(1), s(2)]);
    }

    #[test]
    fn an_unreachable_address_is_dropped_from_the_rest_of_the_knock() {
        let mut net = Recorder {
            fail: vec![(V6, libc::ENETUNREACH)],
            ..Recorder::default()
        };
        let table = Mutex::new(Recent::new());
        let k = Knock::new(knock("1,2").sequence, Duration::ZERO, Duration::ZERO, None).unwrap();
        let out = send_with(&mut net, &table, &[V6, V4], &k).unwrap();
        assert_eq!(out.knocked, [V4]);
        use Event::Tcp;
        assert_eq!(
            net.events,
            [
                Tcp(SocketAddr::new(V6, 1)),
                Tcp(SocketAddr::new(V4, 1)),
                Tcp(SocketAddr::new(V4, 2)),
            ]
        );
        // The dropped address was not knocked, so it is not reused either.
        assert!(!table.lock().unwrap().fresh(V6, &k.sequence, Instant::now()));
    }

    #[test]
    fn every_address_unreachable_is_an_error() {
        for errno in [
            libc::ENETUNREACH,
            libc::EHOSTUNREACH,
            libc::EADDRNOTAVAIL,
            libc::EAFNOSUPPORT,
        ] {
            let mut net = Recorder {
                fail: vec![(V4, errno), (V6, errno)],
                ..Recorder::default()
            };
            let table = Mutex::new(Recent::new());
            let err = send_with(&mut net, &table, &[V4, V6], &knock("1,2")).unwrap_err();
            assert!(
                err.starts_with("no address can be reached (2001:db8::1: "),
                "{err}"
            );
            // Nothing after the first knock, not even its delay.
            assert_eq!(net.events.len(), 2, "{:?}", net.events);
        }
        let mut net = Recorder::default();
        let table = Mutex::new(Recent::new());
        assert_eq!(
            send_with(&mut net, &table, &[], &knock("1")).unwrap_err(),
            "no address to knock"
        );
    }

    #[test]
    fn any_other_error_fails_the_knock() {
        let mut net = Recorder {
            fail: vec![(V4, libc::EMFILE)],
            ..Recorder::default()
        };
        let table = Mutex::new(Recent::new());
        let err = send_with(&mut net, &table, &[V6, V4], &knock("1,2")).unwrap_err();
        assert!(err.starts_with("192.0.2.1: "), "{err}");
    }

    #[test]
    fn a_second_knock_within_2s_is_reused() {
        let mut net = Recorder::default();
        let table = Mutex::new(Recent::new());
        let k = knock("7000,8000");
        send_with(&mut net, &table, &[V4], &k).unwrap();
        let first = net.events.len();
        let out = send_with(&mut net, &table, &[V4], &k).unwrap();
        assert_eq!(
            out,
            Outcome {
                knocked: vec![],
                reused: vec![V4],
                ago: out.ago,
            }
        );
        assert!(out.ago.is_some_and(|a| a < REUSE), "{out:?}");
        assert_eq!(net.events.len(), first, "reused, so nothing sent or waited");
        // A new address alongside is knocked; the reused one is not.
        let out = send_with(&mut net, &table, &[V4, V6], &k).unwrap();
        assert_eq!(out.knocked, [V6]);
        assert_eq!(out.reused, [V4]);
        // A different sequence to the same address is not reused.
        let out = send_with(&mut net, &table, &[V4], &knock("7000,8001")).unwrap();
        assert_eq!(out.knocked, [V4]);
    }

    #[test]
    fn the_reuse_table_forgets_after_2s() {
        let mut t = Recent::new();
        let a = knock("1,2").sequence;
        let b = knock("1,3").sequence;
        let t0 = Instant::now();
        t.note(V4, &a, t0);
        assert!(t.fresh(V4, &a, t0));
        assert!(t.fresh(V4, &a, t0 + REUSE - Duration::from_millis(1)));
        assert!(!t.fresh(V4, &a, t0 + REUSE));
        assert!(!t.fresh(V4, &b, t0));
        assert!(!t.fresh(V6, &a, t0));
        // A later knock renews the entry rather than adding one, and an old
        // one is pruned.
        t.note(V4, &a, t0 + Duration::from_secs(1));
        assert_eq!(t.entries.len(), 1);
        t.note(V6, &a, t0 + Duration::from_secs(5));
        assert_eq!(t.entries.len(), 1);
        assert!(t.fresh(V6, &a, t0 + Duration::from_secs(6)));
    }

    /// Accept one connection on `l` within `within`, or none.
    fn accept(l: &TcpListener, within: Duration) -> Option<TcpStream> {
        l.set_nonblocking(true).unwrap();
        let until = Instant::now() + within;
        loop {
            match l.accept() {
                Ok((s, _)) => return Some(s),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("accept: {e}"),
            }
            if Instant::now() >= until {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether `s`, the listener's end of a knock, is closed at the other
    /// end: end-of-file or a reset, at once.
    fn closed(mut s: TcpStream) -> bool {
        s.set_nonblocking(false).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        match s.read(&mut [0u8; 1]) {
            Ok(0) => true,
            Err(e) => e.kind() == io::ErrorKind::ConnectionReset,
            Ok(_) => false,
        }
    }

    /// What a TCP listener on loopback sees of a knock depends on the
    /// kernel: one that completes the handshake inside `connect()` hands
    /// the listener a connection the knock has already closed; macOS leaves
    /// the socket in SYN_SENT, and the close discards it before the
    /// listener sees anything. Either is a knock closed at once; a second
    /// connection, or one still open, never is.
    fn at_most_one_closed_connection(l: &TcpListener) {
        if let Some(s) = accept(l, Duration::from_millis(200)) {
            assert!(closed(s), "a knock to {:?} is still open", l.local_addr());
            assert!(
                accept(l, Duration::from_millis(50)).is_none(),
                "a second knock on {:?}",
                l.local_addr()
            );
        }
    }

    #[test]
    fn knocks_real_sockets_on_loopback() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let t1 = TcpListener::bind((lo, 0)).unwrap();
        let t2 = TcpListener::bind((lo, 0)).unwrap();
        let u = UdpSocket::bind((lo, 0)).unwrap();
        let port = |a: SocketAddr| a.port();
        let (p1, p2, pu) = (
            port(t1.local_addr().unwrap()),
            port(t2.local_addr().unwrap()),
            port(u.local_addr().unwrap()),
        );
        let k = Knock::new(
            vec![(p1, Proto::Tcp), (pu, Proto::Udp), (p2, Proto::Tcp)],
            Duration::from_millis(30),
            Duration::from_millis(40),
            None,
        )
        .unwrap();
        let table = Mutex::new(Recent::new());
        let started = Instant::now();
        let out = send_with(&mut Sockets, &table, &[lo], &k).unwrap();
        let took = started.elapsed();
        assert_eq!(out.knocked, [lo]);
        // Two delays and the wait, with room for a slow machine.
        assert!(took >= Duration::from_millis(100), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");

        for l in [&t1, &t2] {
            at_most_one_closed_connection(l);
        }
        // The UDP socket got exactly one empty datagram.
        u.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(u.recv(&mut buf).unwrap(), 0);
        u.set_nonblocking(true).unwrap();
        assert_eq!(
            u.recv(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn a_closed_port_on_loopback_is_still_a_knock() {
        // Bound and dropped, so nothing listens there: refused, which means
        // the SYN left.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(Sockets.tcp(SocketAddr::new(lo, port)).is_ok());
        assert!(Sockets.udp(SocketAddr::new(lo, port)).is_ok());
    }

    /// Regression guard for §4.1's load-bearing close: a knock socket left
    /// open retransmits its SYN and resets knockd's sequence. On loopback a
    /// socket left open completes its handshake on any kernel, so the
    /// listener would accept a connection that stays open; after `send`
    /// returns there must be none.
    #[test]
    fn no_knock_socket_is_left_open_after_send() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let l = TcpListener::bind((lo, 0)).unwrap();
        let k = Knock::new(
            vec![(l.local_addr().unwrap().port(), Proto::Tcp); 3],
            Duration::ZERO,
            Duration::ZERO,
            None,
        )
        .unwrap();
        send_with(&mut Sockets, &Mutex::new(Recent::new()), &[lo], &k).unwrap();
        while let Some(s) = accept(&l, Duration::from_millis(200)) {
            assert!(closed(s), "a knock socket is still open after send");
        }
    }

    fn route(out: &str) -> Route {
        Route::parse(out)
    }

    /// §3.2, §5: the knock goes where ssh -G says ssh connects; through a
    /// proxy it is refused unless `knock.host` names the host to knock, and
    /// an ssh -G that could not say is never guessed at.
    #[test]
    fn the_target_is_where_ssh_connects_or_knock_host() {
        let k = knock("7000,8000");
        let direct = route("hostname gate.example.com\nport 22\n");
        assert_eq!(target(&k, "gated", Some(&direct)), Ok("gate.example.com"));
        let jump = route("hostname inner\nproxyjump bastion\n");
        assert_eq!(
            target(&k, "gated", Some(&jump)),
            Err(Refusal::Config(
                "knock: gated is reached through ProxyJump bastion, so a knock from here would \
                 open it to the wrong address; set knock.host to knock the jump host, or knock \
                 from the jump host's ProxyCommand"
                    .into()
            ))
        );
        let e = target(&k, "gated", Some(&route("proxycommand nc %h %p\n"))).unwrap_err();
        assert!(
            matches!(&e, Refusal::Config(m) if m.contains("through a ProxyCommand")),
            "{e:?}"
        );
        assert_eq!(
            target(&k, "gated", None),
            Err(Refusal::Unreachable(
                "knock: cannot tell where ssh connects for gated (ssh -G failed)".into()
            ))
        );
        // knock.host is sent whether or not the dial is proxied.
        let mut via = k.clone();
        via.host = Some("bastion.example.com".into());
        for r in [Some(&jump), Some(&direct), None] {
            assert_eq!(target(&via, "inner", r), Ok("bastion.example.com"));
        }
    }

    /// §3.3: `addressfamily` keeps the addresses ssh may connect to.
    #[test]
    fn the_address_family_keeps_what_ssh_may_dial() {
        assert_eq!(by_family(vec![V4, V6], AddressFamily::Any), [V4, V6]);
        assert_eq!(by_family(vec![V4, V6], AddressFamily::Inet), [V4]);
        assert_eq!(by_family(vec![V4, V6], AddressFamily::Inet6), [V6]);
    }

    /// §7: `-v` names the host, its addresses and the count — never a
    /// port — and says when a knock was reused.
    #[test]
    fn the_v_lines_never_name_a_port() {
        let report = |target: &str, k: &Knock, o: &Outcome| {
            let mut lines = Vec::new();
            report(target, k, o, &mut |l| lines.push(l));
            lines
        };
        let k = knock("7001,8002:udp,9003");
        let o = Outcome {
            knocked: vec![V4, V6],
            reused: vec![],
            ago: None,
        };
        assert_eq!(
            report("gate", &k, &o),
            ["knocking gate (192.0.2.1, 2001:db8::1): 3 knocks, 50 ms apart, then 200 ms"]
        );
        let o = Outcome {
            knocked: vec![],
            reused: vec![V4],
            ago: Some(Duration::from_millis(120)),
        };
        assert_eq!(
            report("gate", &k, &o),
            ["gate was knocked 0.1 s ago — not knocking again"]
        );
        let one = Knock::new(
            vec![(7001, Proto::Tcp)],
            DEFAULT_DELAY,
            Duration::ZERO,
            None,
        )
        .unwrap();
        let o = Outcome {
            knocked: vec![V4],
            ..Outcome::default()
        };
        let lines = report("gate", &one, &o);
        assert_eq!(lines, ["knocking gate (192.0.2.1): 1 knock, then 0 ms"]);
        for l in report("gate", &k, &o).iter().chain(&lines) {
            for port in ["7001", "8002", "9003"] {
                assert!(!l.contains(port), "{l}");
            }
        }
        assert!(!hint("gate").contains("7001"));
    }

    /// A knock's own time: its delays, then its wait (§4.3).
    #[test]
    fn a_knock_takes_its_delays_and_its_wait() {
        assert_eq!(knock("1,2,3").duration(), Duration::from_millis(300));
        assert_eq!(knock("1").duration(), DEFAULT_WAIT);
    }

    /// The hint follows only ssh's failures to connect at all (§7).
    #[test]
    fn the_hint_follows_a_timeout_or_a_refusal() {
        for said in [
            "ssh: connect to host gate port 22: Connection timed out",
            "ssh: connect to host gate port 22: Operation timed out",
            "ssh: connect to host gate port 22: Connection refused",
        ] {
            assert!(connect_failed(said.as_bytes()), "{said}");
        }
        assert!(!connect_failed(b"me@gate: Permission denied (publickey)."));
        assert!(!connect_failed(b"Could not resolve hostname gate"));
    }

    /// A send error names the address, never the port (§7).
    #[test]
    fn a_send_error_names_no_port() {
        struct Broken;
        impl Net for Broken {
            fn tcp(&mut self, _: SocketAddr) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(libc::EACCES))
            }
            fn udp(&mut self, _: SocketAddr) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(libc::EACCES))
            }
            fn sleep(&mut self, _: Duration) {}
        }
        let e = send_with(
            &mut Broken,
            &Mutex::new(Recent::new()),
            &[V4],
            &knock("7001"),
        )
        .unwrap_err();
        assert!(e.starts_with("192.0.2.1: "), "{e}");
        assert!(!e.contains("7001"), "{e}");
    }
}
