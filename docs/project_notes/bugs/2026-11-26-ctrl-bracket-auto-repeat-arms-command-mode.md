# 2026-11-26 — Holding Ctrl-] left command mode armed

Symptom: a Ctrl-] held a moment too long, then `x` (or `d`), killed or detached the session
without the user ever double-tapping.

Cause: `Detector::classify` took every kitty escape event except release (type 3) as a press, so
the `:2` auto-repeats counted, and a legacy terminal's repeated `0x1d` bytes are presses too. The
cycle Idle, Held, Command, Idle has period 3, so after a press plus n repeats with n = 1 (mod 3)
the detector sat in Command for the command window (at least 2 s) and the next `x` exited.

Fix: kitty `:2` events are a new `Kind::EscRepeat`, absorbed into a held or armed state and never
counted. For legacy input, a press that arrives while armed (not the command key) is forwarded
and enters `State::Cooldown`, which forwards further presses without arming until a gap of the
escape window passes between them. DESIGN §6.1 says so.

Regression tests (`src/keys.rs`):
`kitty_auto_repeats_of_the_held_key_are_not_presses` and
`legacy_auto_repeat_of_the_held_key_does_not_rearm`.
