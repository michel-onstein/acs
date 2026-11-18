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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::format_timeout;

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
    /// than [`MAX_KNOCKS`], a duration is out of range, or the delays add
    /// up to more than knockd's `seq_timeout` (§4.3).
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
        let n = knock.sequence.len();
        if n == 0 {
            return Err("a knock sequence needs at least one port".into());
        }
        if n > MAX_KNOCKS {
            return Err(format!("'{knock}': {n} knocks, at most {MAX_KNOCKS}"));
        }
        if delay > MAX_DELAY {
            return Err(format!(
                "a knock delay of {} is too long: at most {}",
                format_timeout(delay),
                format_timeout(MAX_DELAY)
            ));
        }
        if wait > MAX_WAIT {
            return Err(format!(
                "a knock wait of {} is too long: at most {}",
                format_timeout(wait),
                format_timeout(MAX_WAIT)
            ));
        }
        let total = delay * (n as u32 - 1);
        if total > MAX_SEQUENCE_TIME {
            return Err(format!(
                "'{knock}': {n} knocks {} apart take {}, more than knockd's {} seq_timeout",
                format_timeout(delay),
                format_timeout(total),
                format_timeout(MAX_SEQUENCE_TIME)
            ));
        }
        Ok(knock)
    }
}

/// The word that drops a configured knock (§6): `knock: none` in the file,
/// `--knock none` and `ACS_KNOCK=none`.
pub const NO_KNOCK: &str = "none";

/// The keys of a knock written as a mapping in the file (§6.1), in the
/// order messages list them.
pub const KNOCK_KEYS: &[&str] = &["sequence", "delay", "wait", "host"];

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
    let base = Knock::parse(parts.next().unwrap_or_default())?;
    let (mut delay, mut wait, mut host) = (base.delay, base.wait, None);
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!(
                "'/{part}': expected /delay=, /wait= or /host= after the sequence"
            ));
        };
        match key.trim() {
            "delay" => delay = parse_delay(value).map_err(|e| format!("delay: {e}"))?,
            "wait" => wait = parse_wait(value).map_err(|e| format!("wait: {e}"))?,
            "host" => host = Some(parse_host(value)?),
            other => {
                return Err(format!(
                    "unknown knock option '{other}' (known: delay, wait, host)"
                ))
            }
        }
    }
    Knock::new(base.sequence, delay, wait, host).map(Some)
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
/// ago. An address dropped as unreachable is in neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub knocked: Vec<IpAddr>,
    pub reused: Vec<IpAddr>,
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
        tcp_knock(to)
    }

    fn udp(&mut self, to: SocketAddr) -> io::Result<()> {
        let local: IpAddr = match to {
            SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        // Dropped, and so closed, on return: an ICMP unreachable that comes
        // back is never read.
        UdpSocket::bind((local, 0))?.send_to(&[], to).map(drop)
    }

    fn sleep(&mut self, d: Duration) {
        thread::sleep(d);
    }
}

/// One TCP knock: a non-blocking connect, closed before this returns.
fn tcp_knock(to: SocketAddr) -> io::Result<()> {
    let family = match to {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    let raw = crate::sys::cvt(unsafe { libc::socket(family, libc::SOCK_STREAM, 0) })?;
    // Owned from here on, so every path out closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    crate::sys::set_cloexec(raw)?;
    crate::sys::set_nonblocking(raw, true)?;
    let (storage, len) = sockaddr(to);
    let r = unsafe {
        libc::connect(
            raw,
            &storage as *const libc::sockaddr_storage as *const libc::sockaddr,
            len,
        )
    };
    let err = (r != 0).then(io::Error::last_os_error);
    // Closed at once: in SYN_SENT this discards the socket before it can
    // retransmit (§4.1).
    drop(fd);
    match err.as_ref().and_then(io::Error::raw_os_error) {
        // Sent, and still on its way; or sent and refused.
        Some(libc::EINPROGRESS) | Some(libc::ECONNREFUSED) => Ok(()),
        Some(_) => Err(err.unwrap()),
        // Completed at once: something listens there.
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
        self.entries.iter().any(|(a, s, at)| {
            *a == addr && s == sequence && now.saturating_duration_since(*at) < REUSE
        })
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
            if table.fresh(a, &knock.sequence, now) {
                outcome.reused.push(a);
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
    let mut last_drop: Option<String> = None;
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
                Err(e) if unreachable(&e) => last_drop = Some(format!("{a}: {e}")),
                Err(e) => return Err(format!("{to}: {e}")),
            }
        }
        live = kept;
        if live.is_empty() {
            // Only reached through a drop, so `last_drop` is set.
            return Err(format!(
                "no address can be reached ({})",
                last_drop.unwrap_or_default()
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

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
        assert_eq!(
            new(5001, 0).unwrap_err(),
            "a knock delay of 5001ms is too long: at most 5s"
        );
        assert_eq!(
            new(0, 10_001).unwrap_err(),
            "a knock wait of 10001ms is too long: at most 10s"
        );
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
        assert!(err.starts_with("192.0.2.1:1: "), "{err}");
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
                reused: vec![V4]
            }
        );
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
}
