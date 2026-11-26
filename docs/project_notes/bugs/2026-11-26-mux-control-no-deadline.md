# 2026-11-26 — `ssh -O check` and `-O exit` could wait forever on a frozen master

`mux::control` spawned `ssh -O check|exit` and waited with no deadline, though the doc comment on `stop` said
"Always bounded". A live master answers even over a dead TCP link, so only a frozen one (SIGSTOP, an
uninterruptible sleep, a debugger) hung it — and with it every dial (`joinable`) and every `stop` after a network
move or on the dial fallback.

`control` now gives the call five seconds (`CONTROL_DEADLINE`), kills the child at the deadline and returns false:
not joinable, not stopped. Callers are unchanged. The comment now says what is true.

Regression test: `a_frozen_master_does_not_hold_the_control_call` in `src/mux.rs`, a fake `ssh` that never exits;
without the fix it never returns.
