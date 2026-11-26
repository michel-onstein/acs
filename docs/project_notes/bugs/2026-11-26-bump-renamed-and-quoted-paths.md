# 2026-11-26 — xtask bump missed shipped code in moved and non-ASCII paths

`cargo xtask bump` read each commit's files from `git show --numstat`, which has rename detection on and
quotes non-ASCII names. A move into `src/` came back as `{scripts => src}/tool.rs` or `a/x.rs => src/x.rs`, and
`src/café.rs` as `"src/caf\303\251.rs"`; `is_shipping` checks for a `src/` prefix, so it missed all three. For a
subject without a Conventional Commits prefix that turned a PATCH into none, and it undercounted the lines
towards the larger-fix (MINOR) threshold.

The files now come from `git show --numstat -z`: every path verbatim and NUL-terminated, a rename as its own
record with the old and the new path. `parse_numstat_z` lists both paths of a rename, the line count on the one
that ships when only one does, so a move into or out of `src/` touches shipped code and counts the lines the move
changes. Rename detection stays on, so a pure move inside `src/` is not counted as a rewrite of the file.

Regression tests in `xtask/src/bump.rs`: `moved_and_non_ascii_paths_count_as_shipped` builds a repository with
a move into `src/` and a non-ASCII path and fails without the fix; `numstat_z_records` covers the parser.
Prevention: ask git for `-z` output rather than parsing its human-readable path forms.
