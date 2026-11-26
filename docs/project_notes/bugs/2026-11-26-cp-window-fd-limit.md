# 2026-11-26 — A large ACS_CP_WINDOW ran a copy out of file descriptors

`ACS_CP_WINDOW` was only floored at 1, never capped, and the receiver held a part open for every `FILE` it
answered. Each header ahead holds its source open on the sender, so a local copy, both ends in one process, held
about two descriptors per window slot: under macOS's soft limit of 256 a window of about 120 began failing files
with "Too many open files" (per file, through `walk.rs` and `open_part`; no crash). An older or differently
configured sender could do the same to a remote receiver.

The window is now clamped to `cp::MAX_WINDOW` (96), and the receiver holds at most `cp::OPEN_PARTS` (64) parts
open: a `FILE` past them is placed but answered `GO` only once a part closes, in header order, instead of being
failed. The sender only waits for the answer, as on a slow disk, so the protocol is unchanged and an older peer
on either side is served (ACSCP_DESIGN §5.3).

Regression tests: `the_window_is_clamped` in `src/cp/mod.rs`, `files_past_the_open_parts_wait_for_their_go` in
`src/cp/receiver.rs`, and `a_huge_window_stays_under_the_descriptor_limit` in `tests/cp_local.rs` (a local
`acscp -r` of 400 files with `ACS_CP_WINDOW=100000` under a soft limit of 256); each fails without the fix.
