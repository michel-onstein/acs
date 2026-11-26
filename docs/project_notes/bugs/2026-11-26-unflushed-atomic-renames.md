# 2026-11-26 — Upgrades and atomic writes were renamed into place unflushed

`acs upgrade` staged the new binary with `io::copy` and renamed it over the old one without `sync_all`, and never
flushed the directory after the rename. The remote install (`acs install finish`) flushed its completed file but not
the directory, and renamed an uploaded slim build without flushing it at all. `sys::write_atomic`, used for every
configuration and state write, flushed nothing. A power loss or kernel crash just after any of them could leave a
zero-length or partial file under the final name on a filesystem that does not order data before the rename (XFS;
ext4 without `auto_da_alloc`).

Each path now flushes the file before the rename (`sys::sync_file`) and renames through `sys::rename_durable`, which
flushes the parent directory afterwards; `symlink_atomic` uses it too. A directory the filesystem cannot flush
(`EINVAL`) is not an error.

fsync cannot be observed from a test, so `sys::durability` records, under `cfg(test)` only, each flush and rename on
the calling thread. Regression tests: `write_atomic_flushes_the_file_before_the_rename_and_the_directory_after` and
`sync_dir_flushes_a_directory_and_reports_a_missing_one` in `src/sys.rs`, and
`a_staged_copy_is_flushed_before_its_rename_and_its_directory_after` in `src/upgrade.rs`, which assert the order:
file flushed, renamed, directory flushed.
