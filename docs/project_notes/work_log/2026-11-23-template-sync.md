# 2026-11-23 — The gate prints one line per passing step, and the logs start

`/sync-conventions` against qjam/project-template brought four things in:

- `scripts/quiet.sh`: every step of `scripts/verify.sh` runs through it, so a
  green run is a column of `ok:` lines instead of all of `cargo test`'s output,
  and a red one shows the lines that explain it. The count of ignored tests
  (acs-hdj4) is read from the kept log as before.
- `scripts/markdownlint.sh`: markdownlint-cli2 pinned to one version, failing
  when it lints no file, with the shared configuration. It found nothing new.
- `scripts/install-hooks.sh` and `scripts/git-hooks/pre-commit`: after the
  installer, a commit on `main` in the shared checkout fails.
- `docs/project_notes/`, with this log and the bug log, held by
  `scripts/check_project_logs.py` in the gate.

The Python suites that come with them (`test_quiet.py`,
`test_check_project_logs.py`) run in the gate. Verified with
`scripts/verify.sh`; the logs check fails when a single `WORK_LOG.md` appears.
