# Bug Log

Bugs worth remembering: the ones that cost real time, produced no error, and would cost the same time again. Each has
a regression test that fails on the bug and passes with the fix.

**One file per bug**, named `YYYY-MM-DD-slug.md`. `ls -r` reads the log newest first, and `grep -r` finds a bug by
its symptom.

```markdown
# 2026-10-24 — pr-wait crashed in a repository with no workflows

The cause, then the fix, then the regression test that fails on the bug and passes with it — named, so the next
reader can run it.
```

Search it before debugging something that looks familiar.

## Why this is a directory and not one file

For the same reason as [the work log](../work_log/README.md): a single file prepended newest-first conflicts between
any two branches that land work. **Add a file; never edit a shared one.** `scripts/check_project_logs.py` pins the
naming and fails if `BUGS.md` comes back.
