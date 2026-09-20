//! The per-session master (DESIGN §4.2, §4.5, §5.2): owns the pty and the
//! child, keeps the output ring, and serves one active client at a time over
//! a unix socket. Started by the proxy with [`spawn`]; runs `acs _master`.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::proto::{self, err, AttachKind, Decoder, Hello, Mode, Msg, StatusInfo, WinSize};
use crate::reconnect::{Health, Liveness};
use crate::resume::{InputDedupe, OutputRing, Read, DEFAULT_RING};
use crate::session::{self, SocketDir};
use crate::sys;

/// Grace between SIGHUP and SIGKILL when a session is killed.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// How often the socket path is checked (tmpfiles may remove it).
/// `ACS_MASTER_REBIND_MS` shortens it for tests.
fn rebind_every() -> Duration {
    std::env::var("ACS_MASTER_REBIND_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(60))
}
/// Stop filling a client's buffer beyond this; the rest waits in the ring.
const OUT_HIGH_WATER: usize = 64 * 1024;
/// Stop reading a client's input while this much is waiting for the pty.
const PTY_IN_HIGH_WATER: usize = 1 << 20;

// ---- logging (debug aid: ACS_MASTER_LOG=<file>) -----------------------------

fn log(msg: std::fmt::Arguments<'_>) {
    if let Some(path) = std::env::var_os("ACS_MASTER_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "[{}] {}", sys::getpid(), msg);
        }
    }
}

macro_rules! mlog {
    ($($t:tt)*) => { log(format_args!($($t)*)) };
}

// ---- starting a master -----------------------------------------------------

/// Start a detached master for `session` in `dir` and wait until its socket
/// is bound. `exe` is the acs binary to run (normally `sys::self_exe()`).
pub fn spawn(exe: &Path, dir: &Path, session: &str) -> io::Result<()> {
    spawn_with_env(exe, dir, session, &[])
}

/// [`spawn`] with extra environment for the master (tests: `ACS_RING`,
/// `ACS_MASTER_REBIND_MS`, `ACS_MASTER_LOG`).
pub fn spawn_with_env(
    exe: &Path,
    dir: &Path,
    session: &str,
    env: &[(&str, &str)],
) -> io::Result<()> {
    let (r, w) = sys::pipe()?;
    let wfd = w.as_raw_fd();
    let mut cmd = Command::new(exe);
    cmd.arg("_master")
        .arg("--dir")
        .arg(dir)
        .arg("--session")
        .arg(session)
        .arg("--ready-fd")
        .arg("3")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if wfd == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            } else if libc::dup2(wfd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(w);
    // The direct child forks the real master and exits at once.
    child.wait()?;
    let mut msg = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = sys::read(r.as_raw_fd(), &mut buf)?;
        if n == 0 {
            break;
        }
        msg.extend_from_slice(&buf[..n]);
    }
    let msg = String::from_utf8_lossy(&msg);
    match msg.trim() {
        "ok" => Ok(()),
        "" => Err(io::Error::other("master exited before it was ready")),
        other => Err(io::Error::other(
            other.trim_start_matches("err: ").to_string(),
        )),
    }
}

// ---- `acs _master` ---------------------------------------------------------

struct Args {
    dir: PathBuf,
    session: String,
    ready_fd: Option<RawFd>,
    ring: usize,
}

fn parse_args(args: &[OsString]) -> Result<Args, String> {
    let mut dir = None;
    let mut session = None;
    let mut ready_fd = None;
    // Output history kept for resume (DESIGN §4.2); `ACS_RING` in bytes.
    let mut ring = std::env::var("ACS_RING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RING);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .and_then(|v| v.to_str())
                .map(str::to_string)
                .ok_or_else(|| format!("{} needs a value", a.to_string_lossy()))
        };
        match a.to_str() {
            Some("--dir") => dir = Some(PathBuf::from(val()?)),
            Some("--session") => session = Some(val()?),
            Some("--ready-fd") => ready_fd = Some(val()?.parse().map_err(|_| "bad --ready-fd")?),
            Some("--ring") => ring = val()?.parse().map_err(|_| "bad --ring")?,
            _ => return Err(format!("unexpected argument {}", a.to_string_lossy())),
        }
    }
    let session = session.ok_or("missing --session")?;
    session::validate_name(&session)?;
    Ok(Args {
        dir: dir.unwrap_or_else(SocketDir::default_path),
        session,
        ready_fd,
        ring: ring.max(4096),
    })
}

fn report(fd: Option<RawFd>, msg: &str) {
    if let Some(fd) = fd {
        let _ = sys::write_all(fd, msg.as_bytes());
        // SAFETY: we own the inherited descriptor and close it once.
        unsafe { libc::close(fd) };
    }
}

/// Entry point of `acs _master` (arguments after the role).
pub fn main(args: &[OsString]) -> ExitCode {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("acs _master: {e}");
            return ExitCode::from(2);
        }
    };
    sys::close_inherited(&args.ready_fd.into_iter().collect::<Vec<_>>());
    // Detach: the proxy waits for this process, the grandchild is the master.
    // SAFETY: single-threaded at this point; the parent only calls _exit.
    match unsafe { libc::fork() } {
        -1 => {
            report(
                args.ready_fd,
                &format!("err: fork: {}\n", io::Error::last_os_error()),
            );
            return ExitCode::from(1);
        }
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }
    // SAFETY: setsid in the new child cannot fail meaningfully here.
    unsafe { libc::setsid() };
    let _ = std::env::set_current_dir("/");
    let _ = sys::signals::ignore(libc::SIGPIPE);
    let _ = sys::signals::ignore(libc::SIGHUP);
    let _ = sys::signals::ignore(libc::SIGTTOU);
    let _ = sys::signals::ignore(libc::SIGTTIN);

    let master = match Master::bind(&args) {
        Ok(m) => m,
        Err(e) => {
            mlog!("bind failed: {e}");
            report(args.ready_fd, &format!("err: {e}\n"));
            return ExitCode::from(1);
        }
    };
    report(args.ready_fd, "ok\n");
    mlog!("master for '{}' ready", args.session);
    match master.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            mlog!("master failed: {e}");
            ExitCode::from(1)
        }
    }
}

// ---- the master proper -----------------------------------------------------

#[derive(PartialEq, Eq)]
enum ConnState {
    /// Waiting for HELLO or STATUS.
    Pending,
    /// The one attached client; `next` is the next output offset to send.
    Active { next: u64 },
    /// Flush what is queued, then close.
    Closing,
    /// Asked to end the session from outside it (an authorized KILL before
    /// any HELLO): held open, and deaf, until the master exits, so the
    /// asker sees when the session is gone.
    Ending,
}

struct Conn {
    stream: UnixStream,
    dec: Decoder,
    out: Vec<u8>,
    state: ConnState,
    identity: String,
    since: u64,
    dead: bool,
    /// Pings the attached client after silence and gives it up when it
    /// stays silent (DESIGN §5.3, acs-ode).
    live: Liveness,
}

impl Conn {
    fn send(&mut self, m: &Msg) {
        m.encode(&mut self.out);
    }

    /// Write as much queued output as the socket takes.
    fn flush(&mut self) {
        while !self.out.is_empty() && !self.dead {
            match self.stream.write(&self.out) {
                Ok(0) => self.dead = true,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => self.dead = true,
            }
        }
    }
}

struct Child {
    pid: i32,
    pty: OwnedFd,
    command: String,
    status: Option<i32>,
    pty_open: bool,
}

struct Master {
    session: String,
    sock_path: PathBuf,
    listener: UnixListener,
    sock_ino: (u64, u64),
    signals: OwnedFd,
    conns: Vec<Conn>,
    child: Option<Child>,
    ring: OutputRing,
    input: InputDedupe,
    pty_in: Vec<u8>,
    /// Input sequence last ACKed to the attached client.
    acked: u64,
    instance: u64,
    created_at: u64,
    creator: String,
    last_identity: String,
    size: WinSize,
    last_activity: Instant,
    kill_deadline: Option<Instant>,
    next_rebind: Instant,
}

/// The pty's foreground process group, if it is one we may signal
/// (`sys::valid_pgrp`: never 0, which a pty without one reports).
fn fg_pgrp(pty: RawFd) -> Option<i32> {
    sys::tcgetpgrp(pty)
        .ok()
        .and_then(|p| sys::valid_pgrp(p, sys::getpgrp()))
}

/// The pty's poll entry. With nothing to wait for it is left out (`fd -1`):
/// Linux reports POLLHUP on a hung-up pty whatever `events` asks for, and
/// while the ring is full that would spin the loop (acs-gkl).
fn pty_pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    sys::pollfd(if events == 0 { -1 } else { fd }, events)
}

/// After the child's exit status is known: whether the pty has nothing
/// more to read. Only a poll that runs its full course afterwards can say
/// so — the poll that saw the status may have been cut short by SIGCHLD, or
/// have returned for another descriptor, while the child's last output was
/// still on its way to the master side (acs-u3c).
fn pty_drained(fd: RawFd) -> bool {
    let mut p = [sys::pollfd(fd, libc::POLLIN)];
    match sys::poll_retry(&mut p, 50) {
        Ok(_) => p[0].revents & libc::POLLIN == 0,
        Err(_) => true,
    }
}

fn bind(path: &Path) -> io::Result<(UnixListener, (u64, u64))> {
    let l = UnixListener::bind(path)?;
    l.set_nonblocking(true)?;
    let m = std::fs::metadata(path)?;
    Ok((l, (m.dev(), m.ino())))
}

impl Master {
    fn bind(args: &Args) -> Result<Master, String> {
        let dir = SocketDir::open_at(args.dir.clone()).map_err(|e| e.to_string())?;
        let sock_path = dir.socket_path(&args.session)?;
        // Check and bind atomically against other masters of this name.
        let _lock = dir.create_lock(&args.session).map_err(|e| e.to_string())?;
        // A stale socket (nobody listening) is replaced; a live one means
        // another master won the race.
        if UnixStream::connect(&sock_path).is_ok() {
            return Err(format!("session '{}' already has a master", args.session));
        }
        let _ = std::fs::remove_file(&sock_path);
        let (listener, sock_ino) =
            bind(&sock_path).map_err(|e| format!("bind {}: {e}", sock_path.display()))?;
        let signals = sys::signals::install(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGINT])
            .map_err(|e| e.to_string())?;
        Ok(Master {
            session: args.session.clone(),
            sock_path,
            listener,
            sock_ino,
            signals,
            conns: Vec::new(),
            child: None,
            ring: OutputRing::new(args.ring),
            input: InputDedupe::default(),
            pty_in: Vec::new(),
            acked: 0,
            instance: sys::random_u64(),
            created_at: sys::unix_now(),
            creator: String::new(),
            last_identity: String::new(),
            size: WinSize::default(),
            last_activity: Instant::now(),
            kill_deadline: None,
            next_rebind: Instant::now() + rebind_every(),
        })
    }

    fn active(&self) -> Option<usize> {
        self.conns
            .iter()
            .position(|c| matches!(c.state, ConnState::Active { .. }) && !c.dead)
    }

    fn run(mut self) -> io::Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        // Without a child after a while nobody is coming: give up.
        let born = Instant::now();
        loop {
            if self.child.is_none()
                && self.conns.is_empty()
                && born.elapsed() > Duration::from_secs(30)
            {
                mlog!("no client ever created the session; exiting");
                self.cleanup();
                return Ok(());
            }
            if let Some(ch) = &self.child {
                if ch.status.is_some() && !ch.pty_open {
                    return self.finish();
                }
            }

            // Refill and flush before deciding what to wait for: a flush that
            // empties the client's buffer must be followed by a refill from
            // the ring, or with the ring full (no pty POLLIN) and the buffer
            // empty (no POLLOUT) nothing would ever wake us.
            self.pump_output();
            for c in &mut self.conns {
                c.flush();
            }
            self.pump_output();

            // Ping the attached client after silence, and give up one that
            // stays silent: a client that vanished without closing (a laptop
            // powered off, no FIN reaching the host) would otherwise hold
            // the pty back for good once the ring is full (acs-ode).
            let mut wake = u64::MAX;
            for c in &mut self.conns {
                if !matches!(c.state, ConnState::Active { .. }) || c.dead {
                    continue;
                }
                match c.live.tick(&mut c.out) {
                    Health::Dead => {
                        mlog!("the client stopped answering: dropping it");
                        c.dead = true;
                    }
                    Health::Ok => wake = wake.min(c.live.next_deadline_ms()),
                }
            }
            self.conns.retain(|c| !c.dead);

            let active = self.active();
            let mut fds = vec![
                sys::pollfd(self.listener.as_raw_fd(), libc::POLLIN),
                sys::pollfd(self.signals.as_raw_fd(), libc::POLLIN),
            ];
            let pty_idx = fds.len();
            let mut pty_events = 0;
            if let Some(ch) = &self.child {
                if ch.pty_open {
                    let room = match active.map(|i| &self.conns[i].state) {
                        Some(ConnState::Active { next }) => self.ring.room_before_overwrite(*next),
                        _ => usize::MAX,
                    };
                    if room > 0 {
                        pty_events |= libc::POLLIN;
                    }
                    if !self.pty_in.is_empty() {
                        pty_events |= libc::POLLOUT;
                    }
                }
                fds.push(pty_pollfd(ch.pty.as_raw_fd(), pty_events));
            }
            let conn_base = fds.len();
            for c in &self.conns {
                let mut ev = 0;
                if self.pty_in.len() < PTY_IN_HIGH_WATER && c.state != ConnState::Closing {
                    ev |= libc::POLLIN;
                }
                if !c.out.is_empty() {
                    ev |= libc::POLLOUT;
                }
                fds.push(sys::pollfd(c.stream.as_raw_fd(), ev));
            }

            let now = Instant::now();
            let mut timeout = self.next_rebind.saturating_duration_since(now);
            if let Some(d) = self.kill_deadline {
                timeout = timeout.min(d.saturating_duration_since(now));
            }
            if wake != u64::MAX {
                let ms = wake.saturating_sub(sys::now_ms());
                timeout = timeout.min(Duration::from_millis(ms));
            }
            // While the child is gone but the pty still drains, poll quickly.
            if self.child.as_ref().is_some_and(|c| c.status.is_some()) {
                timeout = timeout.min(Duration::from_millis(50));
            }
            sys::poll(&mut fds, timeout.as_millis().min(i32::MAX as u128) as i32)?;

            if fds[1].revents != 0 {
                for sig in sys::signals::drain(self.signals.as_raw_fd()) {
                    match sig {
                        libc::SIGCHLD => self.reap(),
                        _ => {
                            mlog!("signal {sig}: killing session");
                            self.start_kill();
                        }
                    }
                }
            }
            self.reap();
            if self.kill_deadline.is_some_and(|d| Instant::now() >= d) {
                self.hard_kill();
            }
            if Instant::now() >= self.next_rebind {
                self.check_socket();
            }
            if fds[0].revents != 0 {
                self.accept();
            }

            if self.child.is_some() && pty_idx < fds.len() && conn_base > pty_idx {
                let re = fds[pty_idx].revents;
                if re & libc::POLLOUT != 0 {
                    self.write_pty();
                }
                if re & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read_pty(&mut buf);
                }
                // A dead child and nothing more to read although we asked:
                // the output is complete (a background job may still hold
                // the pty open, but the session is over) — once a quiet
                // poll after the exit confirms it.
                if let Some(ch) = &mut self.child {
                    let asked = pty_events & libc::POLLIN != 0;
                    if ch.status.is_some()
                        && ch.pty_open
                        && asked
                        && re & libc::POLLIN == 0
                        && pty_drained(ch.pty.as_raw_fd())
                    {
                        ch.pty_open = false;
                    }
                }
            }

            for i in 0..self.conns.len() {
                let re = fds.get(conn_base + i).map(|f| f.revents).unwrap_or(0);
                if re & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read_conn(i, &mut buf);
                }
            }
            self.pump_output();
            for c in &mut self.conns {
                c.flush();
                if c.state == ConnState::Closing && c.out.is_empty() {
                    c.dead = true;
                }
            }
            self.conns.retain(|c| !c.dead);
            if !self.pty_in.is_empty() {
                self.write_pty();
            }
            self.ack_written();
            for c in &mut self.conns {
                c.flush();
            }
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    match sys::peer_uid(stream.as_raw_fd()) {
                        Ok(uid) if uid == sys::getuid() => {}
                        other => {
                            mlog!("refusing connection from uid {other:?}");
                            continue;
                        }
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    self.conns.push(Conn {
                        stream,
                        dec: Decoder::new(),
                        out: Vec::new(),
                        state: ConnState::Pending,
                        identity: String::new(),
                        since: 0,
                        dead: false,
                        live: Liveness::new(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    mlog!("accept: {e}");
                    break;
                }
            }
        }
    }

    fn read_conn(&mut self, i: usize, buf: &mut [u8]) {
        let n = match sys::read(self.conns[i].stream.as_raw_fd(), buf) {
            Ok(0) => {
                mlog!("client closed the connection");
                self.conns[i].dead = true;
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) => {
                mlog!("client connection failed: {e}");
                self.conns[i].dead = true;
                return;
            }
        };
        self.conns[i].live.heard();
        if matches!(self.conns[i].state, ConnState::Closing | ConnState::Ending) {
            // Taken over, detached or refused: whatever it still sends —
            // INPUT, RESIZE, KILL — is no longer for this session (acs-d1v).
            // Reading on lets us see its end of stream.
            return;
        }
        self.conns[i].dec.push(&buf[..n]);
        loop {
            let msg = match self.conns[i].dec.next_msg() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    mlog!("protocol error: {e}");
                    let c = &mut self.conns[i];
                    c.send(&Msg::Error {
                        code: err::BAD_REQUEST,
                        message: e.to_string(),
                    });
                    c.state = ConnState::Closing;
                    break;
                }
            };
            self.handle(i, msg);
            if self.conns[i].dead
                || matches!(self.conns[i].state, ConnState::Closing | ConnState::Ending)
            {
                break;
            }
        }
    }

    fn handle(&mut self, i: usize, msg: Msg) {
        let pending = self.conns[i].state == ConnState::Pending;
        match msg {
            Msg::Hello(h) if pending => self.hello(i, h),
            Msg::Status if pending => {
                let info = self.status();
                let c = &mut self.conns[i];
                c.send(&Msg::StatusReply(info));
                c.state = ConnState::Closing;
            }
            // A KILL from outside the session says who is asking, and is
            // held to the rule a takeover is held to (acs-fbo): a session
            // another identity is attached to answers BUSY unless the asker
            // has already agreed to take it from them. Before this, a five
            // byte frame from any process of this uid ended any session,
            // and a connection that had just been *refused* with BUSY could
            // destroy the very session it was denied. It does not attach on
            // the way — that would send the attached client TAKEOVER, and
            // the session is about to end, not change hands.
            Msg::Kill { identity, force } if pending => {
                if let Some(a) = self.active() {
                    let other = &self.conns[a];
                    if !force && !other.identity.is_empty() && other.identity != identity {
                        mlog!(
                            "kill from {identity} refused: {} is attached",
                            other.identity
                        );
                        let busy = Msg::Busy {
                            identity: other.identity.clone(),
                            since: other.since,
                        };
                        // Stay pending: the asker may agree and retry.
                        self.conns[i].send(&busy);
                        return;
                    }
                }
                mlog!("kill requested from outside the session by {identity}");
                self.conns[i].state = ConnState::Ending;
                self.start_kill();
            }
            _ if pending => {
                let c = &mut self.conns[i];
                c.send(&Msg::Error {
                    code: err::BAD_REQUEST,
                    message: "expected HELLO".into(),
                });
                c.state = ConnState::Closing;
            }
            Msg::Input { seq, bytes } => {
                // A sequence whose end does not fit in a u64 is not a
                // sequence: treat the frame as malformed (acs-hpf).
                let Some(new) = self.input.accept(seq, &bytes) else {
                    let c = &mut self.conns[i];
                    c.send(&Msg::Error {
                        code: err::BAD_REQUEST,
                        message: "input sequence out of range".into(),
                    });
                    c.state = ConnState::Closing;
                    return;
                };
                if !new.is_empty() {
                    self.pty_in.extend_from_slice(new);
                    self.last_activity = Instant::now();
                }
                // No ACK here: it goes out once the bytes have reached the
                // pty, since DESIGN §5.2 lets the client forget what is
                // ACKed and a queued write can still be dropped (acs-evm).
            }
            Msg::Resize(s) => self.resize(s, false),
            Msg::Ping(n) => self.conns[i].send(&Msg::Pong(n)),
            Msg::Detach => {
                mlog!("client detached");
                self.conns[i].state = ConnState::Closing;
            }
            Msg::Kill { identity, .. } => {
                mlog!("kill requested by the attached client {identity}");
                self.start_kill();
            }
            other => mlog!("ignoring unexpected {other:?}"),
        }
    }

    fn hello(&mut self, i: usize, h: Hello) {
        let reject = |c: &mut Conn, code: u16, message: String| {
            c.send(&Msg::Error { code, message });
            c.state = ConnState::Closing;
        };
        if h.proto != proto::PROTO_VERSION {
            let msg = format!(
                "session '{}' runs acs protocol {} but the client speaks {} — finish or kill the session with the matching acs version",
                self.session,
                proto::PROTO_VERSION,
                h.proto
            );
            return reject(&mut self.conns[i], err::PROTO_MISMATCH, msg);
        }
        if h.session != self.session {
            let msg = format!("this is session '{}', not '{}'", self.session, h.session);
            return reject(&mut self.conns[i], err::BAD_REQUEST, msg);
        }
        let exists = self.child.is_some();
        match (h.mode, exists) {
            (Mode::Attach, false) => {
                let msg = format!("no session '{}'", self.session);
                return reject(&mut self.conns[i], err::NO_SESSION, msg);
            }
            (Mode::Create, true) => {
                let msg = format!("session '{}' already exists", self.session);
                return reject(&mut self.conns[i], err::EXISTS, msg);
            }
            _ => {}
        }
        if let Some(a) = self.active() {
            if a != i {
                let other = &self.conns[a];
                if !h.force && !other.identity.is_empty() && other.identity != h.identity {
                    let busy = Msg::Busy {
                        identity: other.identity.clone(),
                        since: other.since,
                    };
                    // Stay pending: the client may retry with `force`.
                    self.conns[i].send(&busy);
                    return;
                }
                mlog!("takeover by {}", h.identity);
                let old = &mut self.conns[a];
                old.send(&Msg::Takeover);
                old.state = ConnState::Closing;
            }
        }

        let mut created = false;
        if !exists {
            if let Err(e) = self.start_child(&h) {
                let msg = format!("cannot start the session: {e}");
                return reject(&mut self.conns[i], err::INTERNAL, msg);
            }
            created = true;
            self.creator = h.identity.clone();
        }

        let (next, kind) = match h.resume {
            Some(r) if r.instance == self.instance => match self.ring.read_from(r.offset, 0) {
                Read::Data(..) => (r.offset, AttachKind::Resumed),
                Read::Gap(_) | Read::Future => (self.ring.end(), AttachKind::Gap),
            },
            _ => (self.ring.end(), AttachKind::Fresh),
        };
        // A new session starts at offset 0 so the first prompt is not lost.
        let next = if created { 0 } else { next };

        let input_seq = self.written_to_pty();
        let c = &mut self.conns[i];
        c.identity = h.identity.clone();
        c.since = sys::unix_now();
        c.state = ConnState::Active { next };
        c.live = Liveness::new();
        c.send(&Msg::Welcome(proto::Welcome {
            proto: proto::PROTO_VERSION,
            session: self.session.clone(),
            instance: self.instance,
            offset: next,
            created,
            kind,
            input_seq,
        }));
        self.last_identity = h.identity;
        self.last_activity = Instant::now();
        // Fresh or gap: the client cleared its screen, so force a redraw
        // even when the size did not change (dtach's -r winch).
        let redraw = !created && kind != AttachKind::Resumed;
        self.resize(h.size, redraw);
    }

    fn start_child(&mut self, h: &Hello) -> io::Result<()> {
        let (pty, slave) = sys::openpty()?;
        if h.size.cols > 0 && h.size.rows > 0 {
            sys::set_winsize(slave.as_raw_fd(), &h.size)?;
            self.size = h.size;
        }
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let (mut cmd, desc) = if h.command.is_empty() {
            let mut c = Command::new(&shell);
            c.arg("-l");
            (c, format!("{shell} -l"))
        } else {
            let mut c = Command::new(&h.command[0]);
            c.args(&h.command[1..]);
            (c, h.command.join(" "))
        };
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
        cmd.current_dir(&home)
            .env("ACS_SESSION", &self.session)
            .env_remove("ACS_SOCKET_DIR")
            .env_remove("ACS_MASTER_LOG");
        if h.term.is_empty() {
            cmd.env("TERM", "xterm-256color");
        } else {
            cmd.env("TERM", &h.term);
        }
        if h.colorterm.is_empty() {
            cmd.env_remove("COLORTERM");
        } else {
            cmd.env("COLORTERM", &h.colorterm);
        }
        let stdio = |fd: &OwnedFd| -> io::Result<Stdio> { Ok(Stdio::from(fd.try_clone()?)) };
        cmd.stdin(stdio(&slave)?)
            .stdout(stdio(&slave)?)
            .stderr(stdio(&slave)?);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Make the pty (now fd 0) our controlling terminal.
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for s in [
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGCHLD,
                    libc::SIGPIPE,
                    libc::SIGTTIN,
                    libc::SIGTTOU,
                ] {
                    libc::signal(s, libc::SIG_DFL);
                }
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        sys::set_nonblocking(pty.as_raw_fd(), true)?;
        mlog!("started '{desc}' as pid {}", child.id());
        self.child = Some(Child {
            pid: child.id() as i32,
            pty,
            command: desc,
            status: None,
            pty_open: true,
        });
        // Dropping std's handle neither waits nor kills; we reap on SIGCHLD.
        drop(child);
        Ok(())
    }

    fn resize(&mut self, s: WinSize, force_redraw: bool) {
        let Some(ch) = &self.child else { return };
        if s.cols > 0 && s.rows > 0 && s != self.size {
            let _ = sys::set_winsize(ch.pty.as_raw_fd(), &s);
            self.size = s;
        } else if force_redraw {
            let pgrp = fg_pgrp(ch.pty.as_raw_fd()).unwrap_or(ch.pid);
            let _ = sys::kill(-pgrp, libc::SIGWINCH);
        }
    }

    fn read_pty(&mut self, buf: &mut [u8]) {
        let Some(ch) = &mut self.child else { return };
        if !ch.pty_open {
            return;
        }
        let room = match self
            .conns
            .iter()
            .find(|c| matches!(c.state, ConnState::Active { .. }) && !c.dead)
        {
            Some(Conn {
                state: ConnState::Active { next },
                ..
            }) => self.ring.room_before_overwrite(*next),
            _ => buf.len(),
        };
        let want = buf.len().min(room);
        if want == 0 {
            return;
        }
        match sys::read(ch.pty.as_raw_fd(), &mut buf[..want]) {
            Ok(0) => ch.pty_open = false,
            Ok(n) => {
                self.ring.push(&buf[..n]);
                self.last_activity = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            // Linux reports EIO once every slave descriptor is closed.
            Err(_) => ch.pty_open = false,
        }
    }

    /// Input bytes the pty has taken: what was accepted, less what is still
    /// queued for it. This, not what was accepted, is what a client may
    /// forget (DESIGN §5.2).
    fn written_to_pty(&self) -> u64 {
        self.input.written() - self.pty_in.len() as u64
    }

    /// Tell the attached client how much of its input the pty has taken.
    fn ack_written(&mut self) {
        let seq = self.written_to_pty();
        if seq == self.acked {
            return;
        }
        self.acked = seq;
        if let Some(i) = self.active() {
            self.conns[i].send(&Msg::Ack { seq });
        }
    }

    /// Drop the input queued for the pty, unwritten: it never reached the
    /// program, so it was never written (acs-evm).
    fn drop_queued_input(&mut self) {
        self.input.rewind(self.pty_in.len());
        self.pty_in.clear();
    }

    fn write_pty(&mut self) {
        let Some(ch) = &self.child else { return };
        if !ch.pty_open {
            self.drop_queued_input();
            return;
        }
        while !self.pty_in.is_empty() {
            match sys::write(ch.pty.as_raw_fd(), &self.pty_in) {
                Ok(0) => break,
                Ok(n) => {
                    self.pty_in.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.drop_queued_input();
                    break;
                }
            }
        }
    }

    /// Move ring data into the active client's buffer.
    fn pump_output(&mut self) {
        let Some(a) = self.active() else { return };
        let end = self.ring.end();
        let c = &mut self.conns[a];
        let ConnState::Active { next } = &mut c.state else {
            return;
        };
        while c.out.len() < OUT_HIGH_WATER && *next < end {
            match self.ring.read_from(*next, proto::MAX_CHUNK) {
                Read::Data(x, y) => {
                    let bytes = [x, y].concat();
                    let offset = *next;
                    *next += bytes.len() as u64;
                    Msg::Data { offset, bytes }.encode(&mut c.out);
                }
                Read::Gap(start) => *next = start,
                Read::Future => break,
            }
        }
    }

    fn reap(&mut self) {
        if let Some(ch) = &mut self.child {
            if ch.status.is_none() {
                if let Ok(Some(st)) = sys::try_wait(ch.pid) {
                    mlog!("child {} exited with status {st:#x}", ch.pid);
                    ch.status = Some(st);
                }
            }
        }
    }

    fn start_kill(&mut self) {
        let Some(ch) = &self.child else {
            self.cleanup();
            std::process::exit(0);
        };
        if ch.status.is_some() {
            return;
        }
        let pgrp = fg_pgrp(ch.pty.as_raw_fd());
        let _ = sys::kill(-ch.pid, libc::SIGHUP);
        if let Some(p) = pgrp.filter(|&p| p != ch.pid) {
            let _ = sys::kill(-p, libc::SIGHUP);
            let _ = sys::kill(-p, libc::SIGCONT);
        }
        let _ = sys::kill(-ch.pid, libc::SIGCONT);
        self.kill_deadline = Some(Instant::now() + KILL_GRACE);
    }

    fn hard_kill(&mut self) {
        self.kill_deadline = None;
        if let Some(ch) = &self.child {
            if ch.status.is_none() {
                mlog!("child ignored SIGHUP: SIGKILL");
                if let Some(p) = fg_pgrp(ch.pty.as_raw_fd()) {
                    let _ = sys::kill(-p, libc::SIGKILL);
                }
                let _ = sys::kill(-ch.pid, libc::SIGKILL);
            }
        }
    }

    fn check_socket(&mut self) {
        self.next_rebind = Instant::now() + rebind_every();
        match std::fs::metadata(&self.sock_path) {
            Ok(m) if (m.dev(), m.ino()) == self.sock_ino => {}
            Ok(_) => mlog!("socket path now belongs to something else; leaving it"),
            Err(_) => {
                // A cleaner may have removed the whole directory: recreate
                // it (with the usual ownership checks) so the session stays
                // reachable instead of lingering unreachable forever.
                if let Some(dir) = self.sock_path.parent() {
                    if let Err(e) = SocketDir::open_at(dir.to_path_buf()) {
                        mlog!("cannot recreate {}: {e}", dir.display());
                        return;
                    }
                }
                match bind(&self.sock_path) {
                    Ok((l, ino)) => {
                        mlog!("socket was removed; re-bound");
                        self.listener = l;
                        self.sock_ino = ino;
                    }
                    Err(e) => mlog!("re-bind failed: {e}"),
                }
            }
        }
    }

    fn status(&self) -> StatusInfo {
        let attached = self.active();
        StatusInfo {
            name: self.session.clone(),
            attached: attached.is_some(),
            identity: attached
                .map(|a| self.conns[a].identity.clone())
                .unwrap_or_else(|| self.last_identity.clone()),
            creator: self.creator.clone(),
            created_at: self.created_at,
            idle_secs: self.last_activity.elapsed().as_secs(),
            command: self
                .child
                .as_ref()
                .map(|c| c.command.clone())
                .unwrap_or_default(),
            size: self.size,
            version: crate::VERSION.to_string(),
            pid: sys::getpid(),
        }
    }

    /// Only remove the socket if it is still the one we bound.
    fn cleanup(&self) {
        if let Ok(m) = std::fs::metadata(&self.sock_path) {
            if (m.dev(), m.ino()) == self.sock_ino {
                let _ = std::fs::remove_file(&self.sock_path);
            }
        }
    }

    /// The child is gone and the pty drained: deliver EXIT and leave.
    fn finish(mut self) -> io::Result<()> {
        let status = self.child.as_ref().and_then(|c| c.status).unwrap_or(0);
        mlog!("session ended with status {status:#x}");
        // Stop accepting new clients right away.
        self.cleanup();
        self.pump_output_all();
        if let Some(a) = self.active() {
            self.conns[a].send(&Msg::Exit { status });
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.conns.iter().any(|c| !c.out.is_empty() && !c.dead) && Instant::now() < deadline {
            for c in &mut self.conns {
                c.flush();
            }
            let mut fds: Vec<_> = self
                .conns
                .iter()
                .filter(|c| !c.out.is_empty() && !c.dead)
                .map(|c| sys::pollfd(c.stream.as_raw_fd(), libc::POLLOUT))
                .collect();
            if !fds.is_empty() {
                sys::poll(&mut fds, 100)?;
            }
        }
        Ok(())
    }

    /// Queue everything left in the ring for the active client, ignoring
    /// the high-water mark (the session is over).
    fn pump_output_all(&mut self) {
        let Some(a) = self.active() else { return };
        let end = self.ring.end();
        let c = &mut self.conns[a];
        let ConnState::Active { next } = &mut c.state else {
            return;
        };
        while *next < end {
            match self.ring.read_from(*next, proto::MAX_CHUNK) {
                Read::Data(x, y) => {
                    let bytes = [x, y].concat();
                    let offset = *next;
                    *next += bytes.len() as u64;
                    Msg::Data { offset, bytes }.encode(&mut c.out);
                }
                Read::Gap(start) => *next = start,
                Read::Future => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (acs-u3c): output the child wrote just before exiting is
    /// seen by the drain check even though an earlier poll saw nothing —
    /// the check polls afresh instead of trusting that poll's revents.
    #[test]
    fn the_drain_check_sees_output_written_after_the_last_poll() {
        let (master, slave) = sys::openpty().unwrap();
        let m = master.as_raw_fd();
        // The poll the loop made: nothing to read yet.
        let mut p = [sys::pollfd(m, libc::POLLIN)];
        assert_eq!(sys::poll(&mut p, 0).unwrap(), 0);
        // The child's last words, then (as far as the loop knows) its exit.
        sys::write_all(slave.as_raw_fd(), b"last line\r\n").unwrap();
        assert!(!pty_drained(m), "output still to read");
        let mut buf = [0u8; 64];
        assert!(sys::read(m, &mut buf).unwrap() > 0);
        // Read out, with the slave still open (a background job): drained.
        assert!(pty_drained(m));
    }

    #[test]
    fn an_idle_pty_is_left_out_of_the_poll() {
        assert_eq!(pty_pollfd(7, 0).fd, -1);
        assert_eq!(pty_pollfd(7, libc::POLLIN).fd, 7);
    }
}
