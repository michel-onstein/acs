# 2026-11-26 — The cp protocol-error flush could hang on a dead link

When the copy state machine halted on a protocol error, `drive` in `src/cp/mod.rs` switched the link to blocking
mode and `write_all`ed the whole queue (up to the high-water mark plus one 64 KiB chunk) with no timeout. On a
dead link whose ssh channel was full, the peer never read, the write never completed, and the process hung
instead of exiting with the error.

The flush now goes through `write_bounded`: the descriptor stays nonblocking, a full channel is waited on with
`poll` for at most two seconds (`ERROR_FLUSH`), and the write then gives up with `TimedOut`. The flush was
already best effort, so the error is ignored and the process reports the protocol error as before.

Regression test: `bounded_write_gives_up_on_a_reader_that_never_reads` in `src/cp/mod.rs` writes 16 MiB into a
socketpair whose reader never reads and asserts the call returns `TimedOut`; it runs on a thread with a ten
second guard, and fails (the flush hangs) against the old blocking `write_all`.
