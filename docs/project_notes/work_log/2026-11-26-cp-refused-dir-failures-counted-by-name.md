# 2026-11-26 — Count every file refused inside a refused directory (acs-efne)

`src/cp/receiver.rs` keys failures inside a refused directory by their path under it, not the bare name, so the
`-v` tally and `DONE` no longer under-count same-named files. Test in the same file. Bug log:
[cp-refused-dir-failures-counted-by-name](../bugs/2026-11-26-cp-refused-dir-failures-counted-by-name.md).
