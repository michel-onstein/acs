# 2026-11-26 — End input-side escape strings on CAN and SUB (acs-yoym)

`src/keys.rs` now leaves an OSC/DCS/APC string in terminal input on CAN and SUB, mirroring the output-side fix of
acs-q4p6; ESC plus another byte is left as it was, for the reasons in the bug log. Test in `src/keys.rs`. Bug log:
[input-string-can-sub](../bugs/2026-11-26-input-string-can-sub.md).
