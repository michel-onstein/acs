# 2026-11-26 — The deaf-master test left its master running for hours

`a_program_not_reading_its_input_does_not_drop_the_client` in `tests/master.rs` runs a program that reads no
input for a while, then a long `head -c`. When the test process dies before it feeds `head` (a gate timeout, a
killed run), the `TempDir` drop never runs, and `head` waits for input for good. A detached session lives on by
design, so `acs _master --session deaf` stayed up, parent pid 1, for 2.5 hours until found.

The test's shell now ends itself after a two-minute watchdog (`(sleep 120; kill $$) &`), which ends the session
and so the master. On a normal run the test now also asserts the master's end: it reads the master's pid, waits
for the exit, drops the client and requires the pid dead within `T` (`assert_master_ends`). After a full
`cargo test --test master`, `ps` shows no `_master` from the worktree's target dir; no other test in the file
leaks.

Regression test: the end assertion in the test above. Killing the test binary mid-run (`kill -9`) left the
master before the watchdog and leaves none after it, once the two minutes are up.
