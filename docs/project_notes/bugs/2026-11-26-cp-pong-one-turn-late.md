# 2026-11-26 — A cp PING was answered a turn late

`drive()` in `src/cp/mod.rs` queued the `PONG` for a `PING` while handling a read, but wrote it only at the poll after
the next `pump`: the far end heard it one turn of the endpoint's work later. acs-qeqj bounded a turn to about 20 ms of
hashing, which hid this, but the answer still depended on how long a turn runs.

A read's output is now flushed straight after the read (and after the messages the loop starts with), before the next
turn's work. A `PING` read in a turn is answered before that turn's work.

Regression test: `a_ping_is_answered_before_the_next_turns_work` in `src/cp/mod.rs` (a drive loop whose every turn
takes 500 ms; the `PONG` took 900 ms before the fix, 400 ms after).
