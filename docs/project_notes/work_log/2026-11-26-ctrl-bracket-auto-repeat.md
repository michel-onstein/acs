# 2026-11-26 — Holding Ctrl-] no longer arms command mode (acs-jo3k)

Kitty auto-repeat events are ignored by the command-mode detector, and legacy repeated bytes go
through a cooldown that forwards them without re-arming. Regression tests in `src/keys.rs`; see
the [bug log entry](../bugs/2026-11-26-ctrl-bracket-auto-repeat-arms-command-mode.md).
