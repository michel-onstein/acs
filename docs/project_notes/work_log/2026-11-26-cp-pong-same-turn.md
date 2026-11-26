# 2026-11-26 — A cp PING is answered in the turn it is read (acs-wo8h)

`drive()` now flushes the output queue right after a read and after the opening messages, so a `PONG` goes out before
the next turn's hashing or reading instead of one turn later. The far end no longer waits a ping interval plus two
turns however long a turn runs. Bug log: `bugs/2026-11-26-cp-pong-one-turn-late.md`.
