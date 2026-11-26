# 2026-11-26 — A copy between two hosts forwarded a file before checking the target was a directory

`acscp h1:a h1:b h2:file`, several sources into what is not a directory, is to be refused before anything reaches
`h2`. The relay in `src/cp/relay.rs` checked the receiver's `WELCOME` at the top of its splice loop, but in the same
pass it read the sender, queued its `FILE` headers for the receiver and flushed them, all before that check. The
receiver, told `target_is_dir=false`, places a `FILE` at the target path, so a small file could be renamed over an
existing file there, or a stray part file left behind. The receiver is dialled first, so its `WELCOME` usually came
first and hid the bug; a slow one let it through. Found in the v0.23.0 source audit (acs-gcx0).

The fix holds the sender unread until the receiver's `WELCOME` has come and been checked: `splice` no longer passes
on what followed the sender's marker up front, and does not poll the sender until `r.welcome` is set.

Regression test: `a_relay_forwards_no_file_before_the_target_says_it_is_a_directory` in `tests/cp_remote.rs`, whose
target host sends its `WELCOME` a second late and records what it is sent; before the fix the recording held two
`FILE` frames.
