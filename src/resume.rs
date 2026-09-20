//! Data structures behind lossless resume (DESIGN §4.2, §5.2).
//!
//! - [`OutputRing`]: the master's record of recent pty output, addressed by a
//!   monotonically increasing byte offset.
//! - [`Unacked`]: the client's input that was sent but not yet acknowledged.
//! - [`InputDedupe`]: the master's filter that writes each input byte once,
//!   however many times a reconnecting client resends it.
//!
//! Input sequence numbers live in the master's input stream: WELCOME tells a
//! client the master's current position, so two clients taking turns never
//! reuse each other's numbers.

use std::collections::VecDeque;

/// Default output history kept for resume.
pub const DEFAULT_RING: usize = 1 << 20;

/// Result of asking the ring for bytes from an offset.
#[derive(Debug, PartialEq, Eq)]
pub enum Read<'a> {
    /// Bytes starting at the requested offset, in at most two slices (the ring
    /// may wrap). Both empty when the offset is the current end.
    Data(&'a [u8], &'a [u8]),
    /// The offset was overwritten; the oldest retained offset is given.
    Gap(u64),
    /// The offset is beyond anything written.
    Future,
}

/// Fixed-capacity ring of pty output with u64 byte offsets.
pub struct OutputRing {
    buf: Vec<u8>,
    cap: usize,
    /// Total bytes ever pushed: the offset of the next byte.
    end: u64,
}

impl OutputRing {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0);
        OutputRing {
            buf: vec![0; cap],
            cap,
            end: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Offset of the next byte to be written.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Oldest offset still held.
    pub fn start(&self) -> u64 {
        self.end.saturating_sub(self.cap as u64)
    }

    pub fn push(&mut self, mut data: &[u8]) {
        if data.len() > self.cap {
            let skip = data.len() - self.cap;
            self.end += skip as u64;
            data = &data[skip..];
        }
        let pos = (self.end % self.cap as u64) as usize;
        let first = data.len().min(self.cap - pos);
        self.buf[pos..pos + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.end += data.len() as u64;
    }

    /// Bytes from `offset` to the end (at most `max`).
    pub fn read_from(&self, offset: u64, max: usize) -> Read<'_> {
        if offset > self.end {
            return Read::Future;
        }
        if offset < self.start() {
            return Read::Gap(self.start());
        }
        let len = ((self.end - offset) as usize).min(max);
        let pos = (offset % self.cap as u64) as usize;
        let first = len.min(self.cap - pos);
        Read::Data(&self.buf[pos..pos + first], &self.buf[..len - first])
    }

    /// How many more bytes may be pushed before data the client at
    /// `client_offset` has not yet received would be overwritten. The master
    /// stops reading the pty at zero while a client is attached (backpressure).
    pub fn room_before_overwrite(&self, client_offset: u64) -> usize {
        let unsent = self.end.saturating_sub(client_offset) as usize;
        self.cap.saturating_sub(unsent)
    }
}

/// Client side: input sent but not yet acknowledged, for resend after a drop.
#[derive(Default)]
pub struct Unacked {
    /// Sequence number of the first byte in `buf`.
    base: u64,
    buf: VecDeque<u8>,
}

impl Unacked {
    pub fn new(base: u64) -> Self {
        Unacked {
            base,
            buf: VecDeque::new(),
        }
    }

    /// Sequence number the next pushed byte gets.
    pub fn next_seq(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// Record bytes about to be sent; returns the sequence of the first one.
    pub fn push(&mut self, bytes: &[u8]) -> u64 {
        let seq = self.next_seq();
        self.buf.extend(bytes);
        seq
    }

    /// The master has written everything before `seq`.
    pub fn ack(&mut self, seq: u64) {
        if seq <= self.base {
            return;
        }
        let n = ((seq - self.base) as usize).min(self.buf.len());
        self.buf.drain(..n);
        self.base += n as u64;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Everything unacknowledged, with the sequence of its first byte.
    pub fn pending(&self) -> (u64, Vec<u8>) {
        (self.base, self.buf.iter().copied().collect())
    }

    /// Re-base after attaching to a master whose input stream is at
    /// `master_seq`. Unacked bytes the master already has are dropped. When
    /// the master is *behind* our base (a new instance, or another client's
    /// input moved it differently) the unacked bytes cannot be placed and are
    /// discarded: resending keystrokes into an unknown state is worse than
    /// losing them.
    pub fn rebase(&mut self, master_seq: u64, same_instance: bool) {
        if same_instance && master_seq >= self.base && master_seq <= self.next_seq() {
            self.ack(master_seq);
        } else {
            self.buf.clear();
            self.base = master_seq;
        }
    }
}

/// Master side: writes each input byte exactly once.
#[derive(Default)]
pub struct InputDedupe {
    written: u64,
}

impl InputDedupe {
    /// Input bytes written so far (the next accepted sequence number).
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Give up on the last `n` accepted bytes: they were queued for the pty
    /// and dropped unwritten, so they were never written and the client is
    /// to send them again (acs-evm).
    pub fn rewind(&mut self, n: usize) {
        self.written = self.written.saturating_sub(n as u64);
    }

    /// The part of an INPUT frame not yet written. A frame that starts beyond
    /// `written` (a client whose bytes were lost in between) is accepted in
    /// full and the counter jumps forward, so the stream never stalls.
    ///
    /// `None` for a frame whose sequence cannot be one: the end of it does
    /// not fit in a `u64` (acs-hpf). The sequence comes off the wire, and
    /// `seq + len` used to be an unguarded addition — release builds have
    /// no overflow checks, so a frame with a sequence near `u64::MAX` wrapped
    /// `end` to a small number, which then travelled into the master's
    /// `written() - pty_in.len()` and underflowed the ACK the client uses to
    /// decide what it may forget. A debug build aborted the master outright.
    pub fn accept<'a>(&mut self, seq: u64, bytes: &'a [u8]) -> Option<&'a [u8]> {
        let end = seq.checked_add(bytes.len() as u64)?;
        if end <= self.written {
            return Some(&[]);
        }
        let skip = self.written.saturating_sub(seq) as usize;
        self.written = end;
        Some(&bytes[skip..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(r: Read<'_>) -> Vec<u8> {
        match r {
            Read::Data(a, b) => [a, b].concat(),
            other => panic!("{other:?}"),
        }
    }

    /// acs-hpf: the sequence comes off the wire, and `seq + len` was an
    /// unguarded addition. Release builds have no overflow checks, so a
    /// frame near `u64::MAX` wrapped the end to a small number, poisoning
    /// the counter the ACK is computed from; a debug build aborted the
    /// master instead.
    #[test]
    fn a_sequence_whose_end_does_not_fit_is_refused() {
        let mut d = InputDedupe::default();
        d.accept(0, b"hello").unwrap();
        let before = d.written();

        assert_eq!(d.accept(u64::MAX, b"abc"), None);
        assert_eq!(d.accept(u64::MAX - 1, b"ab"), None);
        assert_eq!(d.written(), before, "a refused frame moved the counter");

        // The largest frame that does fit is still taken, and the counter
        // lands exactly on the end.
        let mut d = InputDedupe::default();
        assert_eq!(d.accept(u64::MAX - 3, b"abc"), Some(&b"abc"[..]));
        assert_eq!(d.written(), u64::MAX);
        // An empty frame at the very end fits too.
        assert_eq!(d.accept(u64::MAX, b""), Some(&b""[..]));
    }

    /// Regression (acs-evm): input dropped before it reached the pty was
    /// never written, so the counter goes back and the client resends it.
    #[test]
    fn rewinding_undoes_accepted_bytes() {
        let mut d = InputDedupe::default();
        assert_eq!(d.accept(0, b"abcd"), Some(&b"abcd"[..]));
        assert_eq!(d.written(), 4);
        d.rewind(3);
        assert_eq!(d.written(), 1);
        // The client resends from 1; the whole frame is written again but
        // for the byte that did reach the pty.
        assert_eq!(d.accept(0, b"abcd"), Some(&b"bcd"[..]));
        d.rewind(100);
        assert_eq!(d.written(), 0);
    }

    #[test]
    fn ring_reads_back_what_was_pushed() {
        let mut r = OutputRing::new(8);
        r.push(b"abc");
        assert_eq!(collect(r.read_from(0, 100)), b"abc");
        assert_eq!(collect(r.read_from(1, 1)), b"b");
        assert_eq!(collect(r.read_from(3, 100)), b"");
        assert_eq!(r.read_from(4, 100), Read::Future);
    }

    #[test]
    fn ring_wraps_and_reports_gap_exactly_at_the_boundary() {
        let mut r = OutputRing::new(8);
        let data: Vec<u8> = (0..20).collect();
        for chunk in data.chunks(3) {
            r.push(chunk);
        }
        assert_eq!(r.end(), 20);
        assert_eq!(r.start(), 12);
        assert_eq!(r.read_from(11, 100), Read::Gap(12));
        assert_eq!(collect(r.read_from(12, 100)), (12..20).collect::<Vec<u8>>());
        match r.read_from(14, 100) {
            Read::Data(a, b) => {
                assert!(!b.is_empty(), "expected a wrapped read");
                assert_eq!([a, b].concat(), (14..20).collect::<Vec<u8>>());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ring_push_larger_than_capacity_keeps_the_tail() {
        let mut r = OutputRing::new(4);
        r.push(b"0123456789");
        assert_eq!(r.end(), 10);
        assert_eq!(collect(r.read_from(6, 100)), b"6789");
        assert_eq!(r.read_from(5, 100), Read::Gap(6));
    }

    #[test]
    fn room_before_overwrite_tracks_unsent_bytes() {
        let mut r = OutputRing::new(10);
        assert_eq!(r.room_before_overwrite(0), 10);
        r.push(b"1234567");
        assert_eq!(r.room_before_overwrite(0), 3);
        assert_eq!(r.room_before_overwrite(7), 10);
        r.push(b"890");
        assert_eq!(r.room_before_overwrite(0), 0);
    }

    #[test]
    fn unacked_push_ack_pending() {
        let mut u = Unacked::new(100);
        assert_eq!(u.push(b"hel"), 100);
        assert_eq!(u.push(b"lo"), 103);
        u.ack(102);
        assert_eq!(u.pending(), (102, b"llo".to_vec()));
        u.ack(50);
        assert_eq!(u.pending(), (102, b"llo".to_vec()));
        u.ack(105);
        assert!(u.is_empty());
        assert_eq!(u.next_seq(), 105);
    }

    #[test]
    fn unacked_rebase_same_instance_drops_what_the_master_has() {
        let mut u = Unacked::new(10);
        u.push(b"abcdef");
        u.rebase(13, true);
        assert_eq!(u.pending(), (13, b"def".to_vec()));
    }

    #[test]
    fn unacked_rebase_elsewhere_discards() {
        let mut u = Unacked::new(10);
        u.push(b"abc");
        u.rebase(0, false);
        assert!(u.is_empty());
        assert_eq!(u.next_seq(), 0);
        let mut u = Unacked::new(10);
        u.push(b"abc");
        u.rebase(40, true);
        assert!(u.is_empty());
        assert_eq!(u.next_seq(), 40);
    }

    #[test]
    fn dedupe_writes_each_byte_once() {
        let mut d = InputDedupe::default();
        assert_eq!(d.accept(0, b"abc"), Some(&b"abc"[..]));
        // A resend that partially overlaps.
        assert_eq!(d.accept(1, b"bcde"), Some(&b"de"[..]));
        // A full duplicate.
        assert_eq!(d.accept(0, b"abcde"), Some(&b""[..]));
        assert_eq!(d.written(), 5);
        // A jump forward is accepted whole.
        assert_eq!(d.accept(9, b"xy"), Some(&b"xy"[..]));
        assert_eq!(d.written(), 11);
    }

    #[test]
    fn client_and_master_agree_after_a_drop() {
        // Client sends 6 bytes; the master only saw the first 4 before the drop.
        let mut client = Unacked::new(0);
        let mut master = InputDedupe::default();
        let seq = client.push(b"ls -l");
        let seq2 = client.push(b"\r");
        let seen = master.accept(seq, &b"ls -l"[..4]).unwrap().to_vec();
        assert_eq!(seen, b"ls -");
        let _ = seq2;
        // Reconnect: WELCOME says input_seq = 4.
        client.rebase(master.written(), true);
        let (s, bytes) = client.pending();
        let mut written = seen;
        written.extend_from_slice(master.accept(s, &bytes).unwrap());
        assert_eq!(written, b"ls -l\r");
    }
}
