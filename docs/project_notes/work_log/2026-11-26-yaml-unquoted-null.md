# 2026-11-26 — Read an unquoted ~ or null in the config as unset (acs-whco)

`src/yaml.rs` now reads an unquoted `~`, `null`, `Null` or `NULL` as `Value::Null` instead of the text, so
`identity_file: ~` and `user: ~` are unset rather than `-i $HOME` and `-l '~'`. Quoted forms stay strings. Tests
in `src/yaml.rs` and `src/config.rs`. Bug log: [yaml-unquoted-null](../bugs/2026-11-26-yaml-unquoted-null.md).
