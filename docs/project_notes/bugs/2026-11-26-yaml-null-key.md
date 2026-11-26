# 2026-11-26 — A flow-mapping key spelled `null` or `~` was an error

Since [acs-whco](2026-11-26-yaml-unquoted-null.md) an unquoted `~`, `null`, `Null` or `NULL` reads as
`Value::Null`. In a flow mapping the key goes through the same reader, so `{null: x}` failed with "a mapping
key must be a single value" where it used to read the key as the text `null`. The block path (`null: x`) was
never affected: its keys are split as text and never become a null.

A plain null-spelled key in a flow mapping now reads as the string key it spells, the same as the block form.
acs has no use for a null key.

Regression test: `null_spelled_key_is_a_string_key` in `src/yaml.rs`, covering both forms; it fails without the
fix.
