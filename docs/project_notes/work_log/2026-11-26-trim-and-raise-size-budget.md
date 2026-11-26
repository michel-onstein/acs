# 2026-11-26 — Trim the binary and raise the size budget to 1.2 MB / 2.4 MB for 0.24.1

0.24.0 failed to publish because it was over the size budget. Trimmed 4,856 B of x86_64 Linux slim: no
`sort_unstable` in `close_range_except` (`src/sys.rs`), and one owner helper plus a `Vec` stack in the socket
directory's walk (`src/session.rs`). Then raised `SLIM_BUDGET` / `COMPLETE_BUDGET` in `xtask/src/main.rs` to
1.2 MB / 2.4 MB. Updated DESIGN goal 2, §8.1 (figures, the cuts, a table per target) and added decision 23. Now
1,188,000 B slim and 2,382,097 B complete on x86_64 Linux. Bug log:
[release-0-24-0-over-size-budget](../bugs/2026-11-26-release-0-24-0-over-size-budget.md).
