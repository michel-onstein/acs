# Work Log

What landed when. **One file per landed change**, named `YYYY-MM-DD-slug.md`. `ls -r` reads the log newest first,
and `grep -r acs-xxx .` finds the entry for a bead.

Each entry states what the change accomplished, rather than what it touched:

```markdown
# 2026-10-24 — A duplicate bead title is refused

acs-abc. One paragraph on the cause, then what the change does and what proves it. Name the tests, and the
measurement if there was one.
```

## Why this is a directory and not one file

A log kept as one file with entries prepended newest-first makes its top the hottest line in the repository: every
branch that lands work inserts at the same anchor, so any two branches conflict there, however unrelated their
entries. In qjam-org/md1 two sessions working in parallel livelocked on exactly that — one re-merged `main` seven times
without converging — for about five million tokens over three days, landing nothing.

One file per entry cannot conflict: two branches write two paths, and git merges them without being asked. **Add a
file; never edit a shared one.** `scripts/check_project_logs.py` pins the naming and fails if `WORK_LOG.md` comes back.
