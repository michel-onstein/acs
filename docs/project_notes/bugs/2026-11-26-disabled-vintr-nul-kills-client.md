# 2026-11-26 — A disabled interrupt key made NUL kill the client on a redial

`RawMode::intr()` returned `c_cc[VINTR]` unchecked, and the dial loop treated that byte in the typed input as an
interrupt. With `stty intr undef` on Linux the value is 0, so a NUL from Ctrl-Space or Ctrl-@ typed during a redial
raised `Left::Interrupt` and the client exited 130. On macOS the disabled value is 0xff.

`intr()` now returns `Option<u8>`, `None` when the entry is `_POSIX_VDISABLE` (`tty::control_char`), and the dial loop
asks `tty::has_interrupt`, which never matches when the key is disabled.

Regression test: `nul_is_not_an_interrupt_when_vintr_is_disabled` in `src/tty.rs`, with `disabled_control_char_is_none`.
