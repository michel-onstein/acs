# 2026-11-26 — `--persist` gave up on a first link that was accepted and then silent

On the first connection, a host that sent its ready marker and then nothing (no WELCOME within the handshake
deadline) made the client close the link, write "no answer from …" and return `Exit(UNREACHABLE)` straight from the
session loop. The neighbouring first-connection failures, a dial error and a pre-WELCOME EOF or protocol error, return
`LinkLost` and let the reconnect loop decide, which gives up only when `!persist`. So `--persist` (or `persist: true`)
waited for a host that refused the dial but quit on one that answered and stalled.

The loop now keeps the note and returns `lost(link, LinkEnd::Silent)` on a first connection as it already did on a
redial; `reconnect.rs` exits 255 without persisting and retries when persisting.

Regression test: `a_persisting_first_link_that_goes_silent_is_retried` in `tests/persist.rs`; run against the old code
the client exits 255 instead of starting the session.
