# 2026-11-26 — Count moved and non-ASCII paths when xtask bump classifies commits (acs-gs6v)

`xtask/src/bump.rs` reads a commit's files from `git show --numstat -z`, so a move into or out of `src/` and a
non-ASCII path under it count as shipped code. Tests in `xtask/src/bump.rs`; `docs/VERSIONING.md` says how a
move counts. Bug log: [bump-renamed-and-quoted-paths](../bugs/2026-11-26-bump-renamed-and-quoted-paths.md).
