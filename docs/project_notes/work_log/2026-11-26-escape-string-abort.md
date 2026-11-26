# 2026-11-26 — End output escape strings on CAN, SUB and ESC like a terminal (acs-q4p6)

`ModeObserver` in `src/modes.rs` now leaves an OSC/DCS/APC string on CAN, SUB and ESC followed by a byte other than
`\` or ESC, lexing that byte as after any ESC; ESC ESC stays inside. Bug log:
[escape-string-abort](../bugs/2026-11-26-escape-string-abort.md).
