//! Helpers shared by unit and integration tests. Not part of the tool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory under `/tmp` removed on drop. Kept short on purpose: unix
/// socket paths inside it must stay under ~104 bytes.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    #[allow(clippy::new_without_default)]
    pub fn new() -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(
            "/tmp/acst-{}-{}-{:x}",
            crate::sys::getpid(),
            n,
            crate::sys::random_u64() as u16
        ));
        std::fs::create_dir(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A session's master outlives its test otherwise: end every session
        // whose socket lives here before the directory goes.
        end_sessions_under(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A child process killed and reaped when dropped, so a test that panics
/// part way does not leave it running (acs-88a8). Derefs to the `Child`.
pub struct KillOnDrop(pub std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Both are no-ops on a child already waited for: std remembers the
        // status and signals nothing, so a recycled pid is never hit.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// SIGTERM the master of every session socket at most two levels below
/// `root`; a master ends its session on SIGTERM (SIGHUP, then SIGKILL).
pub fn end_sessions_under(root: &Path) {
    let mut socks = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() && depth < 2 {
                dirs.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x == "sock") {
                socks.push(p);
            }
        }
    }
    for s in socks {
        if let Ok(pid) = session_pid(&s) {
            let _ = crate::sys::kill(pid as i32, libc::SIGTERM);
        }
    }
}

/// The pid of the master behind a session socket, asked via STATUS.
pub fn session_pid(sock: &Path) -> std::io::Result<u32> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(sock)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    s.write_all(&crate::proto::Msg::Status.to_bytes())?;
    let mut dec = crate::proto::Decoder::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Ok(Some(crate::proto::Msg::StatusReply(info))) = dec.next_msg() {
            return Ok(info.pid);
        }
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        dec.push(&buf[..n]);
    }
}

/// CPU time (user + system) process `pid` has used so far: from `/proc` on
/// Linux, `ps` elsewhere. For tests that a process waits rather than spins.
pub fn cpu_time(pid: u32) -> Option<std::time::Duration> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // After "(comm)": state is field 3, utime 14, stime 15.
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        return Some(std::time::Duration::from_millis(ticks * 1000 / hz));
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    // [[dd-]hh:]mm:ss[.cc]
    let text = String::from_utf8_lossy(&out.stdout);
    let t = text.trim();
    let (days, t) = match t.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().ok()?, rest),
        None => (0.0, t),
    };
    let mut secs = 0.0;
    for part in t.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(std::time::Duration::from_secs_f64(days * 86_400.0 + secs))
}

/// CPU time `pid` burns over `span`.
pub fn cpu_over(pid: u32, span: std::time::Duration) -> std::time::Duration {
    let before = cpu_time(pid).expect("cpu time of the process");
    std::thread::sleep(span);
    let after = cpu_time(pid).expect("cpu time of the process");
    after.saturating_sub(before)
}

use crate::proto::{Decoder, Hello, Mode, Msg, Welcome, WinSize, PROTO_VERSION};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A HELLO with test defaults.
pub fn hello(session: &str, mode: Mode, identity: &str) -> Hello {
    Hello {
        proto: PROTO_VERSION,
        session: session.into(),
        mode,
        identity: identity.into(),
        force: false,
        term: "xterm-256color".into(),
        colorterm: String::new(),
        size: WinSize {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        },
        resume: None,
        command: Vec::new(),
    }
}

/// A framed connection for driving a master (unix socket) or a proxy (child
/// pipes) directly. A reader thread feeds a channel so receives can time out
/// on any kind of stream.
pub struct FrameConn {
    writer: Box<dyn Write + Send>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    eof: bool,
    dec: Decoder,
    /// Socket to shut down on drop (wakes the reader thread, tells the peer).
    socket: Option<UnixStream>,
    /// Output bytes received in DATA frames, and the offset after them.
    pub output: Vec<u8>,
    pub next_offset: Option<u64>,
}

impl FrameConn {
    pub fn connect(path: &Path) -> std::io::Result<FrameConn> {
        let s = UnixStream::connect(path)?;
        let mut c = FrameConn::from_io(s.try_clone()?, s.try_clone()?);
        c.socket = Some(s);
        Ok(c)
    }

    pub fn from_io(
        mut reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> FrameConn {
        let (tx, rx) = std::sync::mpsc::channel();
        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = paused.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                while p.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Vec::new());
                        return;
                    }
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        FrameConn {
            writer: Box::new(writer),
            paused,
            rx,
            eof: false,
            dec: Decoder::new(),
            socket: None,
            output: Vec::new(),
            next_offset: None,
        }
    }

    /// Stop (or restart) reading from the peer, to act as a stalled client.
    /// A read already in progress completes first.
    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn send(&mut self, m: &Msg) {
        self.send_raw(&m.to_bytes());
    }

    /// Send without panicking when the peer has already closed.
    pub fn try_send(&mut self, m: &Msg) -> std::io::Result<()> {
        self.writer.write_all(&m.to_bytes())?;
        self.writer.flush()
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("send");
        self.writer.flush().expect("flush");
    }

    /// Close our sending side (the peer sees end of stream).
    pub fn close_write(&mut self) {
        self.writer = Box::new(std::io::sink());
        if let Some(s) = &self.socket {
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    }

    /// Next chunk of raw bytes; `None` on timeout or end of stream.
    fn chunk(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        if self.eof {
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(v) if v.is_empty() => {
                self.eof = true;
                None
            }
            Ok(v) => Some(v),
            Err(_) => None,
        }
    }

    /// Read until the ACS-READY / ACS-NEED marker; frames after it are kept.
    pub fn expect_marker(&mut self, timeout: Duration) -> Option<crate::proto::Marker> {
        let deadline = Instant::now() + timeout;
        let mut sc = crate::proto::MarkerScanner::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let bytes = self.chunk(left)?;
            if let Some(m) = sc.push(&bytes) {
                if let crate::proto::Marker::Ready { rest, .. } = &m {
                    self.dec.push(rest);
                }
                return Some(m);
            }
        }
    }

    /// Next message, or `None` on timeout or end of stream. DATA frames are
    /// also appended to `output` (checking they are contiguous).
    pub fn recv(&mut self, timeout: Duration) -> Option<Msg> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(m) = self.dec.next_msg().expect("valid frames") {
                // Answer the master's PING as a client does (DESIGN §5.3).
                if let Msg::Ping(n) = m {
                    let _ = self.try_send(&Msg::Pong(n));
                    continue;
                }
                if let Msg::Data { offset, bytes } = &m {
                    if let Some(n) = self.next_offset {
                        assert_eq!(*offset, n, "DATA frames must be contiguous");
                    }
                    self.next_offset = Some(offset + bytes.len() as u64);
                    self.output.extend_from_slice(bytes);
                }
                return Some(m);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let bytes = self.chunk(left)?;
            self.dec.push(&bytes);
        }
    }

    /// Receive until a non-DATA/ACK/PONG message arrives.
    pub fn recv_control(&mut self, timeout: Duration) -> Option<Msg> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left)? {
                Msg::Data { .. } | Msg::Ack { .. } | Msg::Pong(_) => continue,
                other => return Some(other),
            }
        }
    }

    /// Receive until `output` contains `needle`; panics on timeout. Only
    /// new bytes are searched, so long outputs in small frames stay linear.
    pub fn wait_output(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let n = needle.as_bytes();
        let mut from: usize = 0;
        loop {
            let start = from.saturating_sub(n.len());
            if self.output[start..].windows(n.len().max(1)).any(|w| w == n) {
                return;
            }
            from = self.output.len();
            let left = deadline.saturating_duration_since(Instant::now());
            if self.recv(left).is_none() && (self.eof || Instant::now() >= deadline) {
                panic!(
                    "{} waiting for {needle:?}; got {:?}",
                    if self.eof {
                        "stream ended"
                    } else {
                        "timed out"
                    },
                    String::from_utf8_lossy(&self.output)
                );
            }
        }
    }

    /// True once the peer closed the stream (after draining `timeout`).
    pub fn closed(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.eof {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if let Some(b) = self.chunk(left) {
                self.dec.push(&b);
            }
        }
        true
    }
}

impl Drop for FrameConn {
    fn drop(&mut self) {
        if let Some(s) = &self.socket {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl FrameConn {
    /// A connection over a spawned `acs _proxy`'s pipes, read past its
    /// ACS-READY, which must name this protocol. Takes the child's stdin and
    /// stdout, so both must have been piped.
    pub fn ready_over(child: &mut std::process::Child) -> FrameConn {
        let out = child.stdout.take().expect("the child's stdout, piped");
        let inp = child.stdin.take().expect("the child's stdin, piped");
        let mut c = FrameConn::from_io(out, inp);
        match c.expect_marker(T) {
            Some(crate::proto::Marker::Ready { proto, .. }) => assert_eq!(proto, PROTO_VERSION),
            other => panic!("expected ACS-READY, got {other:?}"),
        }
        c
    }

    /// The next control message, which must be WELCOME.
    pub fn expect_welcome(&mut self) -> Welcome {
        match self.recv_control(T) {
            Some(Msg::Welcome(w)) => w,
            other => panic!("expected WELCOME, got {other:?}"),
        }
    }

    /// Send `h` and expect WELCOME for it.
    pub fn greet(&mut self, h: Hello) -> Welcome {
        self.send(&Msg::Hello(h));
        self.expect_welcome()
    }

    /// The next control message within `timeout`, which must be EXIT; its
    /// raw wait status.
    pub fn expect_exit(&mut self, timeout: Duration) -> i32 {
        match self.recv_control(timeout) {
            Some(Msg::Exit { status }) => status,
            other => panic!("expected EXIT, got {other:?}"),
        }
    }
}

/// A HELLO that attaches to `session` or creates it running `command`.
pub fn hello_running(session: &str, identity: &str, command: &[&str]) -> Hello {
    let mut h = hello(session, Mode::AttachOrCreate, identity);
    h.command = command.iter().map(|s| s.to_string()).collect();
    h
}

/// Start a master for `name` in `dir` from the acs at `exe`, with `env`,
/// and create the session running `command` as `identity`: the connection
/// that created it and its WELCOME.
pub fn start_master(
    exe: &Path,
    dir: &Path,
    name: &str,
    command: &[&str],
    identity: &str,
    env: &[(&str, &str)],
) -> (FrameConn, Welcome) {
    crate::master::spawn_with_env(exe, dir, name, env).expect("spawn master");
    let mut c = FrameConn::connect(&dir.join(format!("{name}.sock"))).expect("connect to master");
    let w = c.greet(hello_running(name, identity, command));
    (c, w)
}

/// The deadline test helpers wait to. Generous on purpose (acs-kip): the
/// whole suite runs its targets in parallel, so a laptop building and
/// running two dozen binaries at once takes many times longer over any one
/// step than an idle machine does. A test that truly hangs still fails,
/// only later; one that merely lost the CPU for a second does not, and a
/// gate that cries wolf teaches people to re-run rather than to read.
pub const T: Duration = Duration::from_secs(30);

/// How long [`poll`] sleeps between looks.
pub const POLL_EVERY: Duration = Duration::from_millis(20);

/// Call `f` until it returns `Some`, and return what it held; fail once
/// `timeout` has passed, with `what()` saying what never happened. `what`
/// is called only then, so it can describe the state at the deadline.
#[track_caller]
pub fn poll<R, M: std::fmt::Display>(
    timeout: Duration,
    mut f: impl FnMut() -> Option<R>,
    what: impl FnOnce() -> M,
) -> R {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(r) = f() {
            return r;
        }
        if Instant::now() >= deadline {
            panic!("after {timeout:?}: {}", what());
        }
        std::thread::sleep(POLL_EVERY);
    }
}

/// Wait until `ready` holds, failing after `timeout` as [`poll`] does. For
/// the moments a test needs a state to have been *reached* rather than to
/// have been given time to (acs-kip).
#[track_caller]
pub fn wait_until<M: std::fmt::Display>(
    timeout: Duration,
    mut ready: impl FnMut() -> bool,
    what: impl FnOnce() -> M,
) {
    poll(timeout, || ready().then_some(()), what)
}

/// Wait, up to [`T`], for `pid` to spend under a tenth of a half-second
/// window on the CPU (acs-ci9u). It replaces a fixed settle after the event
/// under test, a deadline for the process's legitimate work that a starved
/// host could miss, so that the work spilled into the window the test then
/// measures. A process that spins never gets here at all.
#[track_caller]
pub fn wait_idle(pid: u32, what: &str) {
    let burnt = std::cell::Cell::new(Duration::ZERO);
    wait_until(
        T,
        || {
            burnt.set(cpu_over(pid, Duration::from_millis(500)));
            burnt.get() < Duration::from_millis(50)
        },
        || {
            format!(
                "{what} never went idle: {:?} of CPU in its last 500 ms",
                burnt.get()
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::KillOnDrop;

    fn alive(pid: u32) -> bool {
        crate::sys::kill(pid as i32, 0).is_ok()
    }

    /// acs-88a8: a test that panics while holding a child must not leave
    /// it running.
    #[test]
    fn a_guarded_child_is_killed_when_the_test_panics() {
        let child = std::process::Command::new("/bin/sleep")
            .arg("100")
            .spawn()
            .unwrap();
        let pid = child.id();
        let r = std::panic::catch_unwind(move || {
            let _g = KillOnDrop(child);
            assert!(alive(pid));
            panic!("deliberate");
        });
        assert!(r.is_err());
        // Reaped as well as killed, so not even a zombie is left.
        assert!(!alive(pid), "child {pid} still running");
    }

    /// `poll` hands back what `f` found, on the try that found it.
    #[test]
    fn poll_returns_what_it_waited_for() {
        let mut tries = 0;
        let got = super::poll(
            super::T,
            || {
                tries += 1;
                (tries == 3).then_some("three")
            },
            || "never",
        );
        assert_eq!((got, tries), ("three", 3));
    }

    /// A wait that never comes true fails, saying what it waited for and
    /// with the state `what` reads at the deadline, not before.
    #[test]
    fn wait_until_fails_at_the_deadline_with_its_message() {
        let looks = std::cell::Cell::new(0);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::wait_until(
                std::time::Duration::from_millis(100),
                || {
                    looks.set(looks.get() + 1);
                    false
                },
                || format!("the moon, after {} looks", looks.get()),
            )
        }));
        let msg = *r.unwrap_err().downcast::<String>().unwrap();
        assert!(msg.starts_with("after 100ms: the moon, after "), "{msg}");
        assert!(looks.get() > 1, "looked {} times", looks.get());
        assert!(msg.ends_with(&format!("{} looks", looks.get())), "{msg}");
    }

    /// A child the test already waited for is left alone by the guard.
    #[test]
    fn a_waited_child_is_not_signalled_again() {
        let mut g = KillOnDrop(
            std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap(),
        );
        assert!(g.wait().unwrap().success());
        drop(g);
    }
}
