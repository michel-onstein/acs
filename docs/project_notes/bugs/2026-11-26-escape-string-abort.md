# 2026-11-26 — An unterminated OSC/DCS in program output pinned the mode observer inside a string

`ModeObserver` (`src/modes.rs`) left an OSC, DCS, APC, PM or SOS string only on BEL or `ESC \`. Real terminals
(xterm, VTE, the VT500 parser) also abort it on CAN (0x18), SUB (0x1a) and on ESC followed by any other byte. One
unterminated string in program output therefore kept `at_boundary` false until the next BEL: command-mode bells were
deferred and then dropped, notes waited, and DECSETs were swallowed, so `leave_sequence` could not reset mouse or
alt-screen reporting on detach.

CAN and SUB now return to ground and drop the string. ESC followed by a byte other than `\` or ESC ends the string
and the byte is lexed as after any ESC (`after_esc`). ESC ESC stays inside the string, since tmux passthrough doubles
its ESCs. There is deliberately no byte cap: sixel DCS and kitty APC graphics are legitimately megabytes.

Not done here: the same CAN/SUB handling on the input side (`src/keys.rs`), left for a follow-up.

Regression tests in `src/modes.rs`: `can_sub_and_esc_x_abort_an_unterminated_string`,
`esc_x_after_a_string_is_lexed_as_after_any_esc`, `a_doubled_esc_stays_inside_the_string` and
`an_aborted_title_is_not_remembered`.
