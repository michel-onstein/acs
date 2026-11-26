# 2026-11-26 — Put a deadline on ssh -O check and exit (acs-g1m4)

`src/mux.rs` bounds `ssh -O check|exit` at five seconds and kills the child past it, reading a timeout as not
joinable or not stopped, so a frozen master no longer hangs a dial. Test in `src/mux.rs`. Bug log:
[mux-control-no-deadline](../bugs/2026-11-26-mux-control-no-deadline.md).
