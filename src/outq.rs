//! A byte queue written to a descriptor from a read cursor, as `Decoder`
//! reads its input, rather than drained from the front: `drain(..n)` after
//! every partial write moved everything still queued down to the start —
//! a megabyte per write, behind a slow peer (acs-m08z in the master,
//! acs-x6os in the proxy's relay). Shared by both and by the client's link,
//! and so is its non-blocking flush loop, [`OutQueue::flush`] (acs-8uxk).
//!
//! The written prefix is let go of when the queue empties — the usual case,
//! and free — or, failing that, once it is past [`COMPACT`] and at least as
//! long as what is still queued, so each byte is moved at most once.

use std::io;
use std::os::fd::RawFd;

use crate::sys;

/// Compact a queue whose written prefix has grown past this (and past what
/// is left).
pub(crate) const COMPACT: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct OutQueue {
    buf: Vec<u8>,
    start: usize,
}

impl From<Vec<u8>> for OutQueue {
    fn from(buf: Vec<u8>) -> Self {
        OutQueue { buf, start: 0 }
    }
}

/// What is still queued, as a buffer of its own: the dial's HELLO tail,
/// which the link carries to `serve` (acs-8uxk).
impl From<OutQueue> for Vec<u8> {
    fn from(mut q: OutQueue) -> Self {
        q.buf.drain(..q.start);
        q.buf
    }
}

impl OutQueue {
    /// Bytes queued and not yet written.
    pub(crate) fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.buf.len()
    }

    /// What is still to be written.
    pub(crate) fn queued(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// `n` bytes of `queued` were written.
    pub(crate) fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
    }

    /// The buffer to append to.
    pub(crate) fn tail(&mut self) -> &mut Vec<u8> {
        if self.start > COMPACT && self.start >= self.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        &mut self.buf
    }

    /// Write `data` to the non-blocking `fd` after what is queued: straight
    /// through when nothing is, queueing only what the descriptor would not
    /// take in one write. A write error queues it all, for
    /// [`OutQueue::flush`] to meet again and report in its place in the loop.
    pub(crate) fn send(&mut self, fd: RawFd, data: &[u8]) {
        let n = match self.is_empty() {
            true => sys::write(fd, data).unwrap_or(0),
            false => 0,
        };
        self.tail().extend_from_slice(&data[n..]);
    }

    /// Write as much as the non-blocking `fd` takes. False when the write
    /// fails, a zero-length one included: the other side is gone. Stopping
    /// at `WouldBlock` is not a failure.
    pub(crate) fn flush(&mut self, fd: RawFd) -> bool {
        while !self.is_empty() {
            match sys::write(fd, self.queued()) {
                Ok(0) => return false,
                Ok(n) => self.consume(n),
                Err(e) => return e.kind() == io::ErrorKind::WouldBlock,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// The queue hands back exactly what was appended, in order, across
    /// partial writes, appends between them and a compaction (acs-m08z).
    #[test]
    fn the_queue_writes_every_byte_once_in_order() {
        let mut q = OutQueue::default();
        let mut sent = Vec::new();
        let mut written = Vec::new();
        let mut b = 0u8;
        for round in 0..200 {
            let chunk: Vec<u8> = (0..1000)
                .map(|_| {
                    b = b.wrapping_add(1);
                    b
                })
                .collect();
            q.tail().extend_from_slice(&chunk);
            sent.extend_from_slice(&chunk);
            // Write less than was appended, so the written prefix grows
            // past COMPACT and the queue never empties on its own.
            let n = q.len().min(700 + round % 7);
            written.extend_from_slice(&q.queued()[..n]);
            q.consume(n);
            assert_eq!(q.len(), sent.len() - written.len());
            assert!(q.start <= COMPACT + 1000, "the written prefix is let go of");
        }
        let n = q.len();
        written.extend_from_slice(q.queued());
        q.consume(n);
        assert!(q.is_empty());
        assert_eq!((q.start, q.buf.len()), (0, 0), "an emptied queue resets");
        assert_eq!(written, sent);
    }

    /// Regression (acs-x6os): with a megabyte queued and written a few
    /// bytes at a time, the queue is compacted only once the written prefix
    /// is as long as what is left — so no byte is moved twice, where a
    /// front drain moved the whole backlog after every write.
    #[test]
    fn a_large_backlog_is_not_moved_on_every_partial_write() {
        let mut q = OutQueue::default();
        q.tail().extend(std::iter::repeat(7u8).take(1 << 20));
        let mut moved = 0;
        while !q.is_empty() {
            q.consume(q.len().min(16));
            let before = q.start;
            q.tail().push(7);
            if q.start < before {
                moved += q.len();
            }
            q.consume(1);
        }
        assert!(moved <= 1 << 20, "moved {moved} bytes to write 1 MiB");
    }

    /// `send` writes straight through while nothing is queued, and queues
    /// only what the descriptor would not take — after which it writes
    /// nothing ahead of the queue.
    #[test]
    fn send_writes_through_and_queues_only_the_rest() {
        use std::io::Read;
        let (r, w) = sys::pipe().unwrap();
        sys::set_nonblocking(w.as_raw_fd(), true).unwrap();
        let mut q = OutQueue::default();
        q.send(w.as_raw_fd(), b"hello");
        assert!(q.is_empty(), "taken whole, nothing queued");
        // Fill the pipe, so the rest of a send has to wait in the queue.
        let big = vec![1u8; 1 << 20];
        q.send(w.as_raw_fd(), &big);
        assert!(!q.is_empty() && q.len() < big.len());
        let queued = q.len();
        q.send(w.as_raw_fd(), b"tail");
        assert_eq!(q.len(), queued + 4, "nothing goes out ahead of the queue");
        // Read it all back while flushing: the order holds.
        let mut f = std::fs::File::from(r);
        let mut got = Vec::new();
        let mut b = vec![0u8; 64 * 1024];
        while got.len() < 5 + big.len() + 4 {
            assert!(q.flush(w.as_raw_fd()));
            let n = f.read(&mut b).unwrap();
            got.extend_from_slice(&b[..n]);
        }
        assert!(q.is_empty());
        let mut want = b"hello".to_vec();
        want.extend_from_slice(&big);
        want.extend_from_slice(b"tail");
        assert!(got == want);
    }

    /// What is left of a partly written queue converts to exactly the
    /// unwritten bytes (the dial's HELLO tail, acs-8uxk).
    #[test]
    fn a_queue_converts_to_what_is_still_queued() {
        let mut q = OutQueue::from(b"hello".to_vec());
        q.consume(2);
        assert_eq!(Vec::from(q), b"llo");
        assert!(Vec::from(OutQueue::default()).is_empty());
    }

    /// A write that fails mid-`send` leaves the data queued for `flush`,
    /// which reports the failure.
    #[test]
    fn a_failed_send_is_left_for_flush_to_report() {
        let (r, w) = sys::pipe().unwrap();
        sys::set_nonblocking(w.as_raw_fd(), true).unwrap();
        // The test harness, like every Rust program, ignores SIGPIPE: the
        // write fails with EPIPE instead.
        drop(r);
        let mut q = OutQueue::default();
        q.send(w.as_raw_fd(), b"lost");
        assert_eq!(q.queued(), b"lost");
        assert!(!q.flush(w.as_raw_fd()));
    }
}
