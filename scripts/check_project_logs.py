#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# First written in md1; an `md1-…` id below is a bead in md1's tracker.
"""The work log and the bug log stay directories of one-file-per-entry
fragments.

Each was a single file whose entries were prepended newest-first, so every
branch that landed work inserted at the same anchor and any two branches
conflicted there. Two parallel sessions livelocked on exactly that: one landed
a change every twenty minutes, and the other re-merged and re-resolved these
files seven times without converging, because the conflict came back faster
than it could be cleared.

A fragment per entry cannot conflict — two branches write two paths. This pins
the naming that keeps each directory chronological, and the absence of the old
single files, because recreating one would quietly restore the conflict class
the split removed.

It reads only markdown, so run it in the job that lints markdown rather than
in a code test suite: md1 had it as a Swift test, which kept
`docs/project_notes/**` in swift.yml's paths, so every PR's work-log entry held
both macOS runners.

  python3 scripts/check_project_logs.py [docs/project_notes]

Exits 0 when every log is well-formed, 1 with one line per problem otherwise.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Each log: the directory it lives in, and the single file it replaced.
LOGS = [("work_log", "WORK_LOG.md"), ("bugs", "BUGS.md")]

NAME = re.compile(r"(\d{4})-(\d{2})-(\d{2})-[a-z0-9]+(?:-[a-z0-9]+)*(?:-\d+)?\.md")

DEFAULT_ROOT = Path(__file__).resolve().parent.parent / "docs" / "project_notes"


def entries(directory: Path) -> list[Path]:
    if not directory.is_dir():
        return []
    return sorted(
        (p for p in directory.iterdir() if p.suffix == ".md" and p.name != "README.md"),
        key=lambda p: p.name,
    )


def problems(root: Path) -> list[str]:
    found = []
    for directory, replaced in LOGS:
        log = root / directory
        items = entries(log)

        if not log.is_dir():
            found.append(f"docs/project_notes/{directory}/ holds one file per entry")
        if not (log / "README.md").is_file():
            found.append(f"{directory}/ should say what it is and why it is a directory")

        if (root / replaced).exists():
            found.append(
                f"docs/project_notes/{replaced} is back. It was split into "
                f"{directory}/ because a shared append target conflicts between "
                "any two branches that both land work; add a fragment instead."
            )

        for entry in items:
            name = entry.name
            match = NAME.fullmatch(name)
            if not match:
                found.append(f"{directory}/{name} is not YYYY-MM-DD-slug.md")
                continue
            year, month, day = (int(g) for g in match.groups())
            if not 2020 <= year <= 2100:
                found.append(f"{directory}/{name} has no plausible year")
            if not 1 <= month <= 12:
                found.append(f"{directory}/{name} has no month")
            if not 1 <= day <= 31:
                found.append(f"{directory}/{name} has no day")

            # The entry's heading is the section's own, promoted from H2 to H1,
            # so it still carries the date the entry landed — and that date
            # must agree with the filename, which is what orders the log.
            heading = entry.read_text(encoding="utf-8").split("\n", 1)[0]
            prefix = f"# {name[:10]} — "
            if not heading.startswith(prefix):
                found.append(
                    f'{directory}/{name} should open with "{prefix}<title>", so the '
                    "heading agrees with the filename that orders the log. "
                    f"Found: {heading}"
                )
            elif len(heading) == len(prefix):
                found.append(f"{directory}/{name} has no title after its date")
    return found


def main(argv: list[str]) -> int:
    if argv[1:2] in (["-h"], ["--help"]):
        print(__doc__)
        return 0
    if len(argv) > 2 or argv[1:2] and argv[1].startswith("-"):
        print("usage: scripts/check_project_logs.py [docs/project_notes]", file=sys.stderr)
        return 64
    root = Path(argv[1]) if len(argv) > 1 else DEFAULT_ROOT
    found = problems(root)
    for problem in found:
        print(problem)
    if not found:
        print(f"ok: {', '.join(d for d, _ in LOGS)} are one file per entry")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
