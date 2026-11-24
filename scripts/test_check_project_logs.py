#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# First written in md1; an `md1-…` id below is a bead in md1's tracker.
"""Tests for scripts/check_project_logs.py.

Each rule is shown to fire on a tree that breaks it and to stay quiet on one
that does not, and the real `docs/project_notes` is checked last. Needs only
Python. Run with:

  python3 scripts/test_check_project_logs.py
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_project_logs  # noqa: E402

GOOD = {
    "work_log/README.md": "# Work log\n",
    "work_log/2026-09-24-first.md": "# 2026-09-24 — First\n\nIt landed.\n",
    "work_log/2026-09-25-second-2.md": "# 2026-09-25 — Second\n",
    "bugs/README.md": "# Bugs\n",
    "bugs/2026-09-20-one.md": "# 2026-09-20 — One\n",
    "bugs/2026-09-21-two.md": "# 2026-09-21 — Two\n",
}


class CheckProjectLogsTests(unittest.TestCase):
    def check(self, changes: dict[str, str | None]) -> list[str]:
        files = {**GOOD, **changes}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for path, text in files.items():
                if text is None:
                    continue
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                (root / path).write_text(text, encoding="utf-8")
            return check_project_logs.problems(root)

    def assertFlags(self, changes: dict[str, str | None], fragment: str) -> None:
        found = self.check(changes)
        self.assertTrue(any(fragment in p for p in found), f"{fragment!r} not in {found}")

    def test_a_well_formed_tree_passes(self):
        self.assertEqual(self.check({}), [])

    def test_a_log_with_no_directory(self):
        self.assertFlags({"bugs/README.md": None, "bugs/2026-09-20-one.md": None, "bugs/2026-09-21-two.md": None},
                         "docs/project_notes/bugs/ holds one file per entry")

    def test_a_new_log_with_one_entry_or_none_passes(self):
        # A new repository starts with an empty bug log and a one-entry work log; md1's rule that a log
        # hold two or more entries assumed a log that had been split from a long single file.
        self.assertEqual(self.check({"bugs/2026-09-20-one.md": None, "bugs/2026-09-21-two.md": None,
                                     "work_log/2026-09-25-second-2.md": None}), [])

    def test_a_log_without_its_readme(self):
        self.assertFlags({"work_log/README.md": None}, "work_log/ should say what it is")

    def test_the_single_file_coming_back(self):
        self.assertFlags({"WORK_LOG.md": "# Work log\n"}, "WORK_LOG.md is back")
        self.assertFlags({"BUGS.md": "# Bugs\n"}, "BUGS.md is back")

    def test_a_name_without_a_date(self):
        self.assertFlags({"work_log/notes.md": "# notes\n"}, "notes.md is not YYYY-MM-DD-slug.md")

    def test_a_name_with_capitals_or_underscores(self):
        self.assertFlags({"bugs/2026-09-22-Bad_Name.md": "# 2026-09-22 — x\n"}, "not YYYY-MM-DD-slug.md")

    def test_an_impossible_date(self):
        self.assertFlags({"bugs/2019-09-22-x.md": "# 2019-09-22 — x\n"}, "no plausible year")
        self.assertFlags({"bugs/2026-13-22-x.md": "# 2026-13-22 — x\n"}, "has no month")
        self.assertFlags({"bugs/2026-09-32-x.md": "# 2026-09-32 — x\n"}, "has no day")

    def test_a_heading_that_disagrees_with_the_filename(self):
        self.assertFlags(
            {"work_log/2026-09-24-first.md": "# 2026-09-23 — First\n"}, 'should open with "# 2026-09-24 — '
        )

    def test_a_heading_with_a_date_and_no_title(self):
        self.assertFlags({"work_log/2026-09-24-first.md": "# 2026-09-24 — \n"}, "has no title after its date")

    def test_the_readme_is_not_an_entry(self):
        self.assertEqual(self.check({"bugs/README.md": "No heading at all\n"}), [])

    def test_the_command_exits_nonzero_on_a_problem(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(check_project_logs.main(["check", tmp]), 1)

    def test_the_real_logs(self):
        self.assertEqual(check_project_logs.problems(check_project_logs.DEFAULT_ROOT), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
