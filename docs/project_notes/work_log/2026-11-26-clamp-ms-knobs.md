# 2026-11-26 — Clamp ACS_*_MS timer knobs to a year (acs-5oh2)

`sys::env_ms` and the new `sys::env_ms_u64` clamp millisecond knobs to `sys::MAX_MS` (a year); the liveness and backoff
arithmetic in `src/reconnect.rs` saturates. A huge `ACS_DEAD_MS` or `ACS_PIPE_UNCLAIMED_MS` no longer wraps or panics.
README's environment section notes the ceiling. Bug log: [huge-ms-knobs-overflow](../bugs/2026-11-26-huge-ms-knobs-overflow.md).
