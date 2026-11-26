# 2026-11-26 — Accept tabs as YAML separators after - and in flow collections (acs-v2in)

`src/yaml.rs` trims spaces and tabs after the dash and skips both in flow collections, so `- \thost: a` and
`[a,\tb]` read as the YAML spec says. Tab indentation is still refused. Test in `src/yaml.rs`. Bug log:
[yaml-tab-after-dash-and-in-flow](../bugs/2026-11-26-yaml-tab-after-dash-and-in-flow.md).
