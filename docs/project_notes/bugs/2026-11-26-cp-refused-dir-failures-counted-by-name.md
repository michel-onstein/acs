# 2026-11-26 — Failures inside a refused directory were counted once per bare name

The receiver counts a failure only the first time its key is told (a new link re-reports failures, on purpose).
Entries inside a refused directory were keyed by their bare name, so `d/a/x` and `e/b/x` counted once: the `-v`
tally and the `DONE` failure count came out low. The exit status was unaffected.

They are now keyed by their path under the refused directory, so each distinct file counts.

Regression test: `same_named_files_in_refused_directories_each_count` in `src/cp/receiver.rs`; it fails
(`DONE 0 3` instead of `DONE 0 6`) without the fix.
