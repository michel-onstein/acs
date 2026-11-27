# 2026-11-26 — Check the size budget in the gate and before a release tags (acs-i3vj)

`cargo xtask size` builds the x86_64 and aarch64 Linux slim binaries and the payload blob with `dist`'s own code
(`slim_builds`, `payload_blob`, `report` in `xtask/src/main.rs`) and fails over `SLIM_BUDGET` or `COMPLETE_BUDGET`.
`cargo xtask bump` runs it on its throwaway worktree after setting the version and before committing, so nothing
is tagged or pushed over budget, a `--dry-run` included; `scripts/verify.sh` runs it as the `size budget` step
(about 25 s after a source change, 2 s otherwise; `ACS_SKIP_SIZE=1` skips it there only). `ACS_SLIM_BUDGET` and
`ACS_COMPLETE_BUDGET` can lower the budget, never raise it. Tests: the comparison and the lowering in
`xtask/src/main.rs`; `an_over_budget_release_is_refused_before_anything_is_tagged` in `xtask/tests/bump.rs`.
Docs: docs/VERSIONING.md "The size check", DESIGN §8.1. Bug log:
[release-0-24-0-over-size-budget](../bugs/2026-11-26-release-0-24-0-over-size-budget.md).
