# 2026-11-26 — A huge `ACS_*_MS` value overflowed the timers built on it

`env_or`/`env_ms` accepted any `u64`. `ACS_DEAD_MS` or `ACS_PING_MS` near `u64::MAX` made `heard + dead_after` wrap in
release (every link dead at once) and panic in debug; `Backoff::step` doubled past 2^63; and the master's
`Instant::now() + unclaimed_after()` / `orphan_after()` panicked even in release.

Millisecond knobs are now clamped to a year (`sys::MAX_MS`) in `sys::env_ms` and the new `sys::env_ms_u64`, which the
reconnect knobs use; `Backoff::new` clamps its base too (the copy in `acs cp` goes through it), and the sums in
`Liveness` and `Backoff::step` saturate.

Regression tests: `a_huge_timer_knob_is_clamped_so_sums_on_it_cannot_overflow` in `src/sys.rs` and
`a_huge_backoff_base_or_deadline_does_not_overflow` in `src/reconnect.rs`; both fail without the fix.
