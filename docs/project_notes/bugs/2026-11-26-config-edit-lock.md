# 2026-11-26 — Two `acs config` edits at once lost the first one

`acs config set`, `unset` and the `host` edits read the configuration, edited the tree and renamed a new file into
place with no lock. Two edits running together both read the old file, both succeeded, and the second rename
silently dropped the first edit. Found in a source audit of v0.23.0 (acs-f6nd).

An edit now holds an exclusive `sys::Flock` on `<file>.lock` beside the file the path leads to (symlinks resolved,
so two links to one file share the lock) from the read to the rename. A lock on the configuration itself would do
nothing: `write_atomic` renames a new inode into its place. The lock file is left in place, since removing it would
let a later edit lock a new inode while an earlier one still held the old.

Prevention: any read-modify-write of a file replaced by rename takes a lock on a separate, stable lock file.

Regression test: `concurrent_edits_of_one_file_all_survive` in `src/config_cmd.rs` — two threads add 40 hosts each
under different aliases and every host must be in the file; it fails without the lock.
