# 2026-11-26 — An unquoted `~` or `null` in the config was read as the text "~"

The plain-scalar branches of the YAML reader (`inline` and the flow parser) always produced a string, so `~`,
`null`, `Null` and `NULL` were literal text. `identity_file: ~` went through `path_value`, where `expand_home`
turned it into `-i $HOME`, and `user: ~` became `-l '~'`. The config code already treats `Value::Null` as unset
for every key, but the reader never produced it.

An unquoted `~`, `null`, `Null` or `NULL` now reads as `Value::Null`. Quoted forms (`'~'`, `"null"`) stay
strings, and text that only contains such a word (`~/x`, `nullable`) is unchanged. A write after a read keeps a
quoted `'~'` a string and an unquoted `~` null.

Regression tests: `unquoted_null_is_null_and_quoted_stays_a_string` in `src/yaml.rs` (including the round trip)
and `an_unquoted_tilde_or_null_is_unset_not_text` in `src/config.rs`; both fail without the fix.
