# 2026-11-27 — scripts/verify.sh ends with one fixed completion line (acs-7v9i)

`scripts/verify.sh` now prints exactly one last line on every way out: `verify: PASS`, `verify: FAIL (<step>)`
with the failing step's label, or `verify: FAIL (interrupted)` on SIGINT or SIGTERM, from an `EXIT` trap, so a
crash or Ctrl-C prints it too. Exit statuses are unchanged (a failing step's own status; 130 and 143 for the
signals), and `== ok` stays just before `verify: PASS`. CLAUDE.md's "Build and verify" bullet says to wait on the
process and, if a log must be polled, to poll only for `^verify: (PASS|FAIL)`. `ACS_VERIFY_STEPS=<file>` swaps
in a test-only step list; `scripts/test_verify.py` asserts the line on a pass, a failing step and an interrupt.
Bug log: [verify-wait-loop-hang](../bugs/2026-11-27-verify-wait-loop-hang.md).

`scripts/quiet.sh` is shared with qjam/project-template and untouched; the change is in acs's own `verify.sh`.
