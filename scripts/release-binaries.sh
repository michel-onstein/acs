#!/bin/sh
# Publish the binaries of a release to GitHub Releases (docs/VERSIONING.md):
# builds `cargo xtask dist` from the exact tag in a throwaway worktree,
# packages one archive per target with SHA256SUMS and notes, and creates the
# release (or replaces its assets if it exists).
#
#   scripts/release-binaries.sh [vX.Y.Z] [--dry-run]
#
# The tag defaults to the newest vX.Y.Z on origin. Needs gh with access to the
# repository (GH_TOKEN works), cargo-zigbuild and zig. --dry-run builds and
# packages but uploads nothing, and prints where the assets are.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)

tag=
dry=0
for a in "$@"; do
    case "$a" in
        --dry-run | -n) dry=1 ;;
        v[0-9]*) tag=$a ;;
        *) echo "usage: scripts/release-binaries.sh [vX.Y.Z] [--dry-run]" >&2; exit 2 ;;
    esac
done

git fetch --quiet --tags origin
if [ -z "$tag" ]; then
    tag=$(git tag --list 'v[0-9]*' --sort=-v:refname | head -n 1)
fi
[ -n "$tag" ] || { echo "no vX.Y.Z tag to release" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "no tag $tag" >&2; exit 1; }
version=${tag#v}

work=$(mktemp -d "${TMPDIR:-/tmp}/acs-release.XXXXXX")
cleanup() {
    git -C "$root" worktree remove --force "$work/src" >/dev/null 2>&1 || true
    [ "$dry" = 1 ] || rm -rf "$work"
}
trap cleanup EXIT

echo "== building $tag"
git worktree add --quiet --detach "$work/src" "$tag"
(cd "$work/src" && cargo xtask dist --out "$work/dist")

# What changed: the pull requests since the previous tag.
prev=$(git tag --list 'v[0-9]*' --sort=-v:refname | grep -A1 -x "$tag" | sed -n 2p)
range=$tag
[ -n "$prev" ] && range="$prev..$tag"
git log --first-parent --format='- %s' "$range" | grep -v '^- chore(release):' > "$work/changes.md" || true
if [ -z "$prev" ]; then
    { echo "First release."; echo; cat "$work/changes.md"; } > "$work/c2"
    mv "$work/c2" "$work/changes.md"
fi

echo "== packaging"
cargo xtask package --dist "$work/dist" --version "$version" --out "$work/assets" \
    --readme "$work/src/README.md" --changes "$work/changes.md" >/dev/null

if [ "$dry" = 1 ]; then
    echo "dry run: assets in $work/assets"
    ls -l "$work/assets"
    exit 0
fi

echo "== publishing $tag"
if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" --clobber
    gh release edit "$tag" --notes-file "$work/assets/NOTES.md"
else
    gh release create "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" \
        --verify-tag --title "acs $version" --notes-file "$work/assets/NOTES.md"
fi
gh release view "$tag" --json url -q .url
