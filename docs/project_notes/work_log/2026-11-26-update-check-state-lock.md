# 2026-11-26 — Lock the update-check state file's read-modify-write (acs-fqcm)

`src/update_check.rs` now holds an `flock` on `update-check.lock` beside the state file around every
read-modify-write, so a client start can no longer undo the `latest` or regression flag a background check just
recorded, nor two clients both show the same message. The lock is polled for at most a second and skipped when it
cannot be had. DESIGN §7.6 says so. Tests in `src/update_check.rs`. Bug log:
[update-check-state-lock](../bugs/2026-11-26-update-check-state-lock.md).
