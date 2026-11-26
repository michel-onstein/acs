# 2026-11-26 — Flush staged binaries and atomic writes around the rename (acs-m6ir)

Added `sys::sync_file`, `sys::sync_dir` and `sys::rename_durable` in `src/sys.rs` and used them in
`Staged::new`/`Staged::commit` (`src/upgrade.rs`), the remote install's renames (`src/install.rs`), `write_atomic` and
`symlink_atomic`, so a crash just after an upgrade or config write cannot leave a zero-length file. A test-only
event recorder checks the order. Bug log: [unflushed-atomic-renames](../bugs/2026-11-26-unflushed-atomic-renames.md).
