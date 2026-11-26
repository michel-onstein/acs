# 2026-11-26 — A deeply nested YAML flow-map key overflowed the stack

The flow parser's depth bound (acs-q4f) lived only in `Flow::node()`. A flow mapping parses its key with
`Flow::value()` directly, so `k: {{{{…` thousands deep recursed without a bound and overflowed the stack; with
`panic = "abort"` in the release profile the client died instead of reporting an error.

The bound now lives in `Flow::value()`, so every entry to the parser, key position included, counts toward
`MAX_DEPTH`.

Regression test: `a_configuration_nested_past_all_reason_is_an_error_not_a_crash` in `src/yaml.rs` now also parses
a `k:` value of 5000 `{` and expects the "nested more than" error.
