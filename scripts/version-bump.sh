#!/bin/sh
# Release the next version automatically (docs/VERSIONING.md): PATCH for a
# small fix, MINOR for a feature or a larger fix, MAJOR only with --major.
#
#   scripts/version-bump.sh [--dry-run] [--major|--minor|--patch]
#
# Safe to re-run: with nothing unreleased it does nothing. Run after a
# merge to main (the ship skill does); it pushes a chore(release) commit
# and a vX.Y.Z tag to origin/main from a throwaway worktree.
set -eu
cd "$(dirname "$0")/.."
exec cargo xtask bump "$@"
