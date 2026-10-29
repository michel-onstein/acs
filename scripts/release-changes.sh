#!/bin/sh
# The "what changed" list for a release's notes (docs/VERSIONING.md): one
# line per pull request that landed since the previous release tag, newest
# first, on stdout. `scripts/release-binaries.sh` packages it into `NOTES.md`.
#
#   scripts/release-changes.sh [TAG]
#
# Tags are vX.Y.Z up to v0.18.0 and X.Y.Z from 0.19.0 on, and "previous" is
# by version across both forms (scripts/release-tags.sh), so 0.19.0's list
# runs from v0.18.0. The tag defaults to the newest this checkout has. With
# no previous tag the list is introduced by "First release.".
#
# Bookkeeping is left out: `chore(release):` is the version bump this very
# release is, and `chore(beads):` only opens and closes issues under
# `.beads/`. Neither says anything about the binary someone is about to
# install, and between them they outnumber the entries that do. Nothing else
# is filtered — a `chore` that touches the build or the scripts stays.
set -eu
cd "$(dirname "$0")/.."

tag=${1:-}
[ -n "$tag" ] || tag=$(scripts/release-tags.sh | head -n 1)
[ -n "$tag" ] || { echo "no release tag to list the changes of" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "no tag $tag" >&2; exit 1; }

prev=$(scripts/release-tags.sh | grep -A1 -xF "$tag" | sed -n 2p)
range=$tag
if [ -n "$prev" ]; then
    range="$prev..$tag"
else
    echo "First release."
    echo
fi
git log --first-parent --format='- %s' "$range" |
    grep -Ev '^- chore\((release|beads)\):' || true
