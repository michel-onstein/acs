# 2026-11-27 — Waiting on verify.sh with a guessed last line hung for 22 hours

Two implement-beads passes ran `scripts/verify.sh` in the background and waited with their own loop,
`until grep -qE "ok: markdownlint|FAIL|failed|error" <output>; do sleep 3; done`. The gate never prints
`ok: markdownlint`, so after the gates passed both loops polled for about 22 hours until stopped by hand. The
script had no fixed last line to wait on, so each caller guessed one.

`verify.sh` now ends every run with `verify: PASS`, `verify: FAIL (<step>)` or `verify: FAIL (interrupted)`, and
CLAUDE.md says to wait on the process, or poll a log only for `^verify: (PASS|FAIL)`.

Regression tests: `CompletionLineTests` in `scripts/test_verify.py` (pass, failing step, failing step's exit
status, SIGINT and SIGTERM); they fail without the fix, which ignores `ACS_VERIFY_STEPS` and prints no line.
