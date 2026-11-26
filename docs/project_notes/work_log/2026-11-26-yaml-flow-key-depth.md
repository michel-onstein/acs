# 2026-11-26 — Bound YAML flow-map key nesting (acs-rc1g)

Moved the nesting-depth check from `Flow::node()` into `Flow::value()` in `src/yaml.rs`, so a flow-mapping key
cannot recurse past `MAX_DEPTH` and overflow the stack. The existing depth regression test gained a key-position
case. Bug log: [yaml-flow-key-depth](../bugs/2026-11-26-yaml-flow-key-depth.md).
