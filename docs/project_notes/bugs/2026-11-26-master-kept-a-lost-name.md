# 2026-11-26 — A master that lost its name after a directory wipe lived on unreachable

When a cleaner removed the whole session directory — socket and `.master` file both — and a client redialled
before the old master's 5 s check, the client started a second master, which took the recreated `.master` lock and
socket. The old master's check then found `MasterFile::take` returning `Ok(None)` and the socket answering for
another master; it logged "now belongs to another master" and "leaving it", and kept its child, pty and ring for
good, re-checking every 5 s. Neither the childless give-up nor the pipes `unclaimed_by` exit applies to a session
with a live shell, so nothing ever ended it.

`check_socket` in `src/master.rs` now calls `stand_down` when the name is demonstrably another master's (the
`.master` lock is held by someone else, or a live master answers on the socket). With no client attached it logs
that the session is unreachable and ends it as a kill does (SIGHUP, then SIGKILL after the grace); a client still
attached keeps it until it leaves, and the next check ends it then. DESIGN §4.1 says so.

Regression test: `a_master_whose_name_was_taken_after_a_wipe_stands_down` in `tests/master.rs` wipes the
directory, starts a second session under the name, prods the old master with `SIGUSR1`, and expects the old shell
and master to exit while the new session stays reachable. It times out on the old code.
