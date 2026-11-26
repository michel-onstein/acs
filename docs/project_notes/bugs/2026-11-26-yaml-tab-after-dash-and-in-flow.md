# 2026-11-26 — A tab after the dash or in a flow collection became part of the key or item

The sequence reader stripped only spaces after the dash, and the flow parser skipped only spaces between items, so
`- \thost: a` gave the key `\thost` and `[a,\tb]` gave the item `\tb`. The config code then refused valid YAML
with a confusing `unknown key '\thost'` or a bad host name.

A tab now separates like a space after `-` (including `-\t` with nothing else before the tab) and in flow
collections. A tab as block indentation is still refused.

Regression test: `tabs_separate_after_a_dash_and_in_flow_collections` in `src/yaml.rs`; it fails without the fix
and also asserts tab indentation stays an error.
