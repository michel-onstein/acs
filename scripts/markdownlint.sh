#!/usr/bin/env bash
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# First written in m1, much of it after md1; an `m1-…` or `md1-…` id below is a bead in that repository's tracker.
#
# Lints every markdown file of the checkout this script lives in, with that checkout's
# .markdownlint-cli2.jsonc, and fails when no file was linted (m1-1m7). Called by
# scripts/verify.sh, locally and in .forgejo/workflows/ci.yml, so both apply the same guard.
#
#   scripts/markdownlint.sh [markdownlint-cli2 arguments…]   # e.g. --fix
#   scripts/markdownlint.sh --help                           # this text
#
# It runs from the checkout's root whatever the caller's directory: markdownlint-cli2 takes
# its globs and ignores from the .markdownlint-cli2.jsonc in its working directory, and
# outside the root it finds none and prints its usage. The config is also passed with
# --config, so a missing one is an error rather than a silent default. markdownlint-cli2
# exits 0 when its globs match nothing, so scripts/lib/lint_summary.py reads the
# `Linting: N files` line and fails when N is 0.
#
# The linter's version is pinned (m1-4sy): scripts/lib/markdownlint-cli2.version holds one
# exact version, so a new upstream rule cannot
# turn an unchanged tree red. ACS_MARKDOWNLINT_CLI2_VERSION overrides it, to try a newer one
# before bumping the file.
set -uo pipefail

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  sed -n '3,/^[^#]/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
  exit 0
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root" || exit 1

if ! command -v npx >/dev/null 2>&1; then
  echo "    npx not found: install node (brew install node)" >&2
  exit 1
fi

version="${ACS_MARKDOWNLINT_CLI2_VERSION:-$(cat "$root/scripts/lib/markdownlint-cli2.version" 2>/dev/null)}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "    markdownlint-cli2 version '$version' is not an exact X.Y.Z (scripts/lib/markdownlint-cli2.version)" >&2
  exit 1
fi

log="$(mktemp "${TMPDIR:-/tmp}/acs-markdownlint.XXXXXX")" || exit 1
# Removed however the script ends, an interrupt too (m1-qjpi).
trap 'rm -f "$log"' EXIT
npx --yes "markdownlint-cli2@$version" --config .markdownlint-cli2.jsonc "$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

python3 "$root/scripts/lib/lint_summary.py" "$log"
summary=$?

((status != 0)) && exit "$status"
exit "$summary"
