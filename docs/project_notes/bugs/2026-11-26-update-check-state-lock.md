# 2026-11-26 — Two processes updating the update-check state could undo each other

`update_check.rs` read, changed and wrote the state file (`~/.local/state/acs/update-check`) with no lock, in
both the client start (`on_client_start`) and the background check (`check_main`). Each write is atomic, but the
read-modify-write is not: a client that read the file just before the check recorded `latest` wrote its stale
copy back afterwards, so the news of a new release was lost for another week; the same race could drop the
`regressed` warning, and two clients starting together could both show the same "acs X is available" message.

Every read-modify-write now goes through `update`, which holds an `flock` on a separate, stable
`update-check.lock` beside the state file (the state file itself is replaced by rename, so locking it would lock
nothing). The lock is polled for at most a second (`LOCK_WAIT`) rather than taken blocking: the guarded section
is a few bytes of I/O, so a free lock comes at once, but a client start must not hang on a holder that was
stopped mid-write. A lock that cannot be had — a read-only directory, a stuck holder — falls back to the old
unlocked behaviour, as a cache can afford.

Regression test: `a_client_start_does_not_undo_a_concurrent_check` in `src/update_check.rs` holds a client
between its read and its write while a check records `latest`; it fails without the lock.
`a_stuck_lock_holder_does_not_hold_up_a_client_start` and `an_unopenable_lock_falls_back_to_no_lock` hold the
fallback.

Prevention: a file that more than one process rewrites needs a lock around the whole read-modify-write, not
just an atomic write.
