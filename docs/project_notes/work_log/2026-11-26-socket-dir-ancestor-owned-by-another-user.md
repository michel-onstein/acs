# 2026-11-26 — Refuse a socket directory under another user's directory, whatever its mode (acs-9imr)

`src/session.rs` now refuses an ancestor of the socket or control directory owned by anyone other than the
user or root, even at `0755`: its owner could rename what is in it. Root's and the user's own trees (`/home`,
`/run/user/<uid>`, macOS's `/Users` and `$TMPDIR`) keep working; DESIGN §4.5 documents the rule. Unit tests
over the made-up file system in `src/session.rs`, a root test in `tests/multiuser.rs`. Bug log:
[socket-dir-ancestor-owned-by-another-user](../bugs/2026-11-26-socket-dir-ancestor-owned-by-another-user.md).
