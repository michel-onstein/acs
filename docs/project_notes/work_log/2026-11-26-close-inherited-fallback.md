# 2026-11-26 — Close inherited descriptors even without /dev/fd (acs-8znv)

`sys::close_inherited` falls back from `/dev/fd` to `/proc/self/fd`, `close_range(2)` and a capped loop to
`RLIMIT_NOFILE` instead of returning silently. Test in `src/sys.rs`. Bug log:
[close-inherited-fallback](../bugs/2026-11-26-close-inherited-fallback.md).
