# 2026-11-26 — Give a redial's prompt its own window (acs-u904)

`src/client.rs`: once ssh takes the terminal for a prompt during a redial, the dial's deadline moves to two
minutes from the hand-off (`dial_deadline`, `ACS_DIAL_TIMEOUT_MS`) instead of the redial's 30 s from the spawn,
so a slow password is no longer killed under the user; an unanswered prompt still ends the dial. DESIGN §5.3 and
decision 18 updated. Tests in `src/client.rs` and `tests/reconnect.rs`. Bug log:
[redial-deadline-under-a-prompt](../bugs/2026-11-26-redial-deadline-under-a-prompt.md).
