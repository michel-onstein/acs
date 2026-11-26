# 2026-11-26 — Lock acs config edits against concurrent writers (acs-f6nd)

`src/config_cmd.rs` now holds a `flock` on `<file>.lock` beside the configuration from the read to the rename, so
two `acs config` edits at once both land instead of the second dropping the first. Test
`concurrent_edits_of_one_file_all_survive`; DESIGN's `acs config` section names the lock. Bug log:
[config-edit-lock](../bugs/2026-11-26-config-edit-lock.md).
