# 2026-11-26 — Judge the socket directory's ancestors through symlinks (acs-vqdo)

`src/session.rs` now resolves a socket or control directory's parent link by link and judges the directories
it really passes through, never a link's own mode: a root-owned `/home -> var/home` no longer makes
`ACS_SOCKET_DIR` unusable, and a writable directory behind a link is refused. A link owned by someone other
than the user or root is refused. Unit tests over a made-up file system in `src/session.rs`, a root test in
`tests/multiuser.rs`. Bug log:
[socket-dir-ancestors-through-symlinks](../bugs/2026-11-26-socket-dir-ancestors-through-symlinks.md).
