# 2026-11-26 — Accept a flow-mapping key spelled null or ~ again (acs-39tf)

`src/yaml.rs` reads a plain `null` or `~` key in `{...}` as the string key it spells, as the block form
already did. Test in `src/yaml.rs`. Bug log: [yaml-null-key](../bugs/2026-11-26-yaml-null-key.md).
