# 2026-11-26 — A slow disk in a copy was read as the far end gone silent

`acs cp` told `Liveness::wrote_for` nothing, though ACSCP_DESIGN §5.2 said it did (only the client's terminal
write, acs-spd, did). `drive` also marked a read heard before handling it. So a local write blocked longer than
`ACS_NETCHECK_MS` (2 s) after a network change gave a spurious `Dead` and a redial, with the `PONG` still unread
in the pipe; and a stall past `ACS_DEAD_MS` inside one read's messages aged the dead interval against pings that
were waiting to be read.

`drive` now takes each read through `took_read`: it times the handling, tells `Liveness::wrote_for`, and marks
the read heard once it is handled. The design's §5.2 is true as written.

Regression test: `a_slow_disk_does_not_fail_an_outstanding_netcheck` in `src/cp/mod.rs`; it fails (`Dead`)
without the `wrote_for` call and passes with it.
