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
