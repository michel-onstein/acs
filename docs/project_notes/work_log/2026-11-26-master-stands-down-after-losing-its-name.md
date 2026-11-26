# 2026-11-26 — A master that lost its name to another stands down (acs-c152)

`check_socket` in `src/master.rs` now ends a session whose name another master took after a whole-directory wipe,
when no client is attached, instead of keeping the unreachable shell, pty and ring for good. DESIGN §4.1 documents
it. Regression test `a_master_whose_name_was_taken_after_a_wipe_stands_down` in `tests/master.rs`. Bug log:
[master-kept-a-lost-name](../bugs/2026-11-26-master-kept-a-lost-name.md).
