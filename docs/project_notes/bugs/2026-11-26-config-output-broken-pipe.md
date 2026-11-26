# 2026-11-26 — `acs config` output to a closed pipe panicked with exit 101

Rust ignores SIGPIPE, so `print!` to a stdout whose reader had gone away (`acs config show | true`) panicked
on EPIPE and exited 101. `acs config` (`config_cmd.rs`), `acs list`, `acs upgrade` help and `acs` usage all
printed this way. They now go through `acs::print_out`, which writes to a stdout lock and ends quietly on any
write error, so a reader that stops early is a success.

Regression test: `output_to_a_closed_pipe_is_not_a_panic` in `tests/config_cmd.rs` (a config large enough to
exceed the pipe buffer, stdout read end closed before it writes); it exited 101 without the fix.
