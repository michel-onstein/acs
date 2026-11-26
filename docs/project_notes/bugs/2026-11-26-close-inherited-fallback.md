# 2026-11-26 — close_inherited silently did nothing without /dev/fd

The defensive cleanup of inherited descriptors (`sys::close_inherited`, called by the proxy and the master daemon)
listed `/dev/fd` and returned quietly when it could not be read, so in a chroot or any setup without it a leaked
descriptor stayed open for the daemon's whole life.

It now tries `/dev/fd`, then `/proc/self/fd`, then `close_range(2)` (Linux, skipping the kept descriptors), then
closes every number from 3 up to `RLIMIT_NOFILE`, capped at 65536. The descriptor directories are a parameter of
`close_inherited_from` so the fallback is testable.

Regression test: `close_inherited_closes_without_a_descriptor_directory` in `src/sys.rs` (in a forked child; fails
without the fix).
