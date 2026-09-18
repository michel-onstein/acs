#!/bin/sh
# Publish the binaries of a release to GitHub Releases (docs/VERSIONING.md):
# builds `cargo xtask dist` from the exact tag in a throwaway worktree,
# packages one archive per target with SHA256SUMS, the one-line installer
# (scripts/install.sh) and notes, and creates the release (or replaces its
# assets if it exists). Then points the Homebrew tap at it
# (scripts/update-tap.sh; ACS_NO_TAP=1 skips that).
#
#   scripts/release-binaries.sh [vX.Y.Z] [--dry-run]
#
# The tag defaults to the newest vX.Y.Z on origin. Needs gh with access to the
# repository and the tap (GH_TOKEN works), cargo-zigbuild and zig. --dry-run
# builds and packages but uploads nothing, prints where the assets are and
# shows what the tap would get.
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
    --readme "$work/src/README.md" --installer "$work/src/scripts/install.sh" \
    --changes "$work/changes.md" >/dev/null
cargo xtask formula --version "$version" --sums "$work/assets/SHA256SUMS" --out "$work/acs.rb"

if [ "$dry" = 1 ]; then
    echo "dry run: assets in $work/assets"
    ls -l "$work/assets"
    [ -n "${ACS_NO_TAP:-}" ] || scripts/update-tap.sh "$work/acs.rb" --dry-run
    exit 0
fi

echo "== publishing $tag"
if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" \
        "$work/assets/install.sh" --clobber
    gh release edit "$tag" --notes-file "$work/assets/NOTES.md"
else
    gh release create "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" "$work/assets/install.sh" \
        --verify-tag --title "acs $version" --notes-file "$work/assets/NOTES.md"
fi
gh release view "$tag" --json url -q .url

[ -z "${ACS_NO_TAP:-}" ] || exit 0
echo "== updating the Homebrew tap"
scripts/update-tap.sh "$work/acs.rb" || {
    echo "the release is published but the tap is not: retry with scripts/update-tap.sh $tag" >&2
    exit 1
}
