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
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

use crate::proto::{Decoder, Hello, Mode, Msg, WinSize, PROTO_VERSION};
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

/// A framed connection for driving a master or proxy directly.
pub struct FrameConn {
    pub stream: UnixStream,
    dec: Decoder,
    /// Output bytes received in DATA frames, and the offset after them.
    pub output: Vec<u8>,
    pub next_offset: Option<u64>,
}

impl FrameConn {
    pub fn connect(path: &Path) -> std::io::Result<FrameConn> {
        Ok(FrameConn {
            stream: UnixStream::connect(path)?,
            dec: Decoder::new(),
            output: Vec::new(),
            next_offset: None,
        })
    }

    pub fn send(&mut self, m: &Msg) {
        self.stream.write_all(&m.to_bytes()).expect("send frame");
    }

    /// Next message, or `None` on timeout or end of stream. DATA frames are
    /// also appended to `output` (checking they are contiguous).
    pub fn recv(&mut self, timeout: Duration) -> Option<Msg> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(m) = self.dec.next_msg().expect("valid frames") {
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
            if left.is_zero() {
                return None;
            }
            // macOS rejects this with EINVAL once the peer has closed;
            // the read below then returns end of stream at once anyway.
            let _ = self.stream.set_read_timeout(Some(left));
            let mut buf = [0u8; 65536];
            match self.stream.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => self.dec.push(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return None
                }
                Err(e) => panic!("recv: {e}"),
            }
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

    /// Receive until `output` contains `needle`; panics on timeout.
    pub fn wait_output(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !String::from_utf8_lossy(&self.output).contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if self.recv(left).is_none() && Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; got {:?}",
                    String::from_utf8_lossy(&self.output)
                );
            }
        }
    }
}
