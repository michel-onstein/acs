# 2026-11-26 — Bound the acscp protocol-error flush with a deadline (acs-6a73)

The protocol-error path in `src/cp/mod.rs` no longer writes its queue in blocking mode without a limit: it polls
for room for at most two seconds and gives up, so a dead link with a full channel cannot hang acscp. Tests in
`src/cp/mod.rs`. Bug log: [cp-error-flush-unbounded](../bugs/2026-11-26-cp-error-flush-unbounded.md).
