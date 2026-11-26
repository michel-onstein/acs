# 2026-11-26 — Ignore a disabled interrupt key during a redial (acs-am6n)

`RawMode::intr()` in `src/tty.rs` now returns `Option<u8>`, `None` for `_POSIX_VDISABLE`, and `src/client.rs` checks
through `tty::has_interrupt`, so NUL typed during a redial no longer ends the client when `intr` is undefined. Bug log:
[disabled-vintr-nul-kills-client](../bugs/2026-11-26-disabled-vintr-nul-kills-client.md).
