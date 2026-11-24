#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# First written in m1, much of it after md1; an `m1-…` or `md1-…` id below is a bead in that repository's tracker.
"""Check a markdownlint-cli2 log, and fail when it linted no file (m1-1m7).

    python3 scripts/lib/lint_summary.py <log>

markdownlint-cli2 exits 0 when its globs match nothing, so a run that linted no
file cannot be told apart from a clean one by its status — the same trap
scripts/lib/test_summary.py closes for a test run that ran no test (m1-eaj).
scripts/markdownlint.sh keeps the output in a file and hands it here.

Its progress lines are

    Linting: 58 files
    Summary: 0 issues in 0 files

and the second count of the Summary line is the number of files *with issues*,
not the number linted: `0 issues in 0 files` is a clean run. Only `Linting:`
says how many files were linted.

Prints `markdownlint: N files linted, M issues`. Exits 1 when there is no
`Linting:` line, when N is 0, or when the Summary reports an issue; 64 on a
usage error. `--help` prints this text.
"""

import re
import sys

LINTING = re.compile(r"^Linting: (\d+) files?$")
SUMMARY = re.compile(r"^Summary: (\d+) issues? in (\d+) files?$")


def summarise(lines):
    """Return (files linted or None, issues reported or None)."""
    linted = issues = None
    for raw in lines:
        line = raw.strip()
        match = LINTING.match(line)
        if match:
            linted = int(match.group(1))
            continue
        match = SUMMARY.match(line)
        if match:
            issues = int(match.group(1))
    return linted, issues


def main(argv):
    if argv[1:] in (["-h"], ["--help"]):
        print(__doc__.strip())
        return 0
    if len(argv) != 2:
        print(f"usage: {argv[0]} <markdownlint-cli2 log>", file=sys.stderr)
        return 64
    with open(argv[1], encoding="utf-8", errors="replace") as log:
        linted, issues = summarise(log)
    if linted is None:
        print("    no 'Linting: N files' line: markdownlint-cli2 linted nothing", file=sys.stderr)
        return 1
    print(f"    markdownlint: {linted} file{'' if linted == 1 else 's'} linted, {issues or 0} issues")
    if linted == 0:
        print("    0 files linted: the globs matched nothing", file=sys.stderr)
        return 1
    if issues:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
