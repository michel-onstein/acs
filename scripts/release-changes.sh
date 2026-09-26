#!/bin/sh
# The "what changed" list for a release's notes (docs/VERSIONING.md): one
# line per pull request that landed since the previous vX.Y.Z tag, newest
# first, on stdout. `scripts/release-binaries.sh` packages it into `NOTES.md`.
#
#   scripts/release-changes.sh [vX.Y.Z]
#
# The tag defaults to the newest vX.Y.Z this checkout has. With no previous
# tag the list is introduced by "First release.".
#
# Bookkeeping is left out: `chore(release):` is the version bump this very
# release is, and `chore(beads):` only opens and closes issues under
# `.beads/`. Neither says anything about the binary someone is about to
# install, and between them they outnumber the entries that do. Nothing else
# is filtered — a `chore` that touches the build or the scripts stays.
set -eu
cd "$(dirname "$0")/.."

tag=${1:-}
[ -n "$tag" ] || tag=$(git tag --list 'v[0-9]*' --sort=-v:refname | head -n 1)
[ -n "$tag" ] || { echo "no vX.Y.Z tag to list the changes of" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "no tag $tag" >&2; exit 1; }

prev=$(git tag --list 'v[0-9]*' --sort=-v:refname | grep -A1 -x "$tag" | sed -n 2p)
range=$tag
if [ -n "$prev" ]; then
    range="$prev..$tag"
else
    echo "First release."
    echo
fi
git log --first-parent --format='- %s' "$range" |
    grep -Ev '^- chore\((release|beads)\):' || true
