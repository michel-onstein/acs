# 2026-11-26 — Honour --persist when the first link goes silent (acs-ddck)

A first connection that is accepted and then silent past the handshake deadline now goes through the reconnect loop
like every other first-link failure, so `--persist` retries it instead of exiting 255. Change in `src/client.rs`; new
test in `tests/persist.rs`. Bug log: [persist-first-link-silent](../bugs/2026-11-26-persist-first-link-silent.md).
