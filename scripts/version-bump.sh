#!/bin/sh
# Release the next version automatically (docs/VERSIONING.md): PATCH for a
# small fix, MINOR for a feature or a larger fix, MAJOR only with --major.
#
#   scripts/version-bump.sh [--dry-run] [--major|--minor|--patch]
#
# Safe to re-run: with nothing unreleased it does nothing. Run after a
# merge to main (the ship skill does); it pushes a chore(release) commit
# and a vX.Y.Z tag to origin/main from a throwaway worktree, then publishes
# that release's binaries to GitHub (scripts/release-binaries.sh; set
# ACS_NO_PUBLISH=1 to skip).
set -eu
cd "$(dirname "$0")/.."

before=$(mktemp)
after=$(mktemp)
trap 'rm -f "$before" "$after"' EXIT
git tag --list 'v[0-9]*' | sort > "$before"

cargo xtask bump "$@"

for a in "$@"; do
    case "$a" in --dry-run | -n | -h | --help) exit 0 ;; esac
done
[ -z "${ACS_NO_PUBLISH:-}" ] || exit 0

git tag --list 'v[0-9]*' | sort > "$after"
for tag in $(comm -13 "$before" "$after"); do
    scripts/release-binaries.sh "$tag"
done
