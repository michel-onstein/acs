# 2026-11-26 — Keep acs cp under the descriptor limit whatever its window (acs-d2kt)

`ACS_CP_WINDOW` is clamped to 96 (`cp::MAX_WINDOW`), and the receiver holds at most 64 parts open
(`cp::OPEN_PARTS`), answering a `FILE` past them once a part closes rather than failing it — compatible with an
older sender, which only waits. ACSCP_DESIGN §5.3, the README's environment table and `acscp --help` say so.
Tests in `src/cp/mod.rs`, `src/cp/receiver.rs` and `tests/cp_local.rs`. Bug log:
[cp-window-fd-limit](../bugs/2026-11-26-cp-window-fd-limit.md).
