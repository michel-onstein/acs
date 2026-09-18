#!/bin/sh
# Update the Homebrew tap (docs/VERSIONING.md, "Homebrew"): commit a rendered
# Formula/acs.rb to michel-onstein/homebrew-acs and push it.
#
#   scripts/update-tap.sh FORMULA [--dry-run]    a file from cargo xtask formula
#   scripts/update-tap.sh vX.Y.Z [--dry-run]     render that release's formula
#
# scripts/release-binaries.sh runs it after publishing a release; run it by
# hand to retry. The tap is cloned into a temporary directory; a formula that
# is unchanged, or older than the tap's, is left alone. --dry-run shows the
# change and pushes nothing. ACS_TAP_REPO names another tap repository (the
# tests use a local one). Pushing to GitHub uses gh's token (GH_TOKEN works).
set -eu

usage() {
    echo "usage: scripts/update-tap.sh FORMULA|vX.Y.Z [--dry-run]" >&2
    exit 2
}

arg=
dry=0
for a in "$@"; do
    case "$a" in
        --dry-run | -n) dry=1 ;;
        -*) usage ;;
        *) [ -z "$arg" ] || usage; arg=$a ;;
    esac
done
[ -n "$arg" ] || usage
repo=${ACS_TAP_REPO:-https://github.com/michel-onstein/homebrew-acs.git}

work=$(mktemp -d "${TMPDIR:-/tmp}/acs-tap.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$arg" in
    v[0-9]*)
        # A version: render its formula from the published SHA256SUMS.
        curl -fsSL -o "$work/SHA256SUMS" \
            "https://github.com/michel-onstein/acs/releases/download/$arg/SHA256SUMS"
        (cd "$(dirname "$0")/.." && cargo xtask formula --version "$arg" \
            --sums "$work/SHA256SUMS" --out "$work/acs.rb")
        formula=$work/acs.rb
        ;;
    *) formula=$arg ;;
esac
[ -f "$formula" ] || { echo "no formula at $formula" >&2; exit 1; }

# The release a formula installs, from its download URLs.
version_of() {
    sed -n 's|.*/releases/download/v\([0-9][0-9.]*\)/.*|\1|p' "$1" | head -n 1
}
version=$(version_of "$formula")
[ -n "$version" ] || { echo "$formula names no release" >&2; exit 1; }

git clone --quiet "$repo" "$work/tap"
tap=$work/tap
current=
[ -f "$tap/Formula/acs.rb" ] && current=$(version_of "$tap/Formula/acs.rb")
if [ -n "$current" ] && [ "$current" != "$version" ]; then
    newest=$(printf '%s\n%s\n' "$current" "$version" | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
    if [ "$newest" = "$current" ]; then
        echo "tap: already at acs $current, newer than $version; left alone"
        exit 0
    fi
fi

mkdir -p "$tap/Formula"
cp "$formula" "$tap/Formula/acs.rb"
git -C "$tap" add Formula/acs.rb
if git -C "$tap" diff --cached --quiet; then
    echo "tap: Formula/acs.rb is already acs $version"
    exit 0
fi
if [ "$dry" = 1 ]; then
    echo "dry run: would commit acs $version to $repo"
    git -C "$tap" --no-pager diff --cached --stat
    exit 0
fi

# gh's token for GitHub, in this clone only: it replaces any other helper,
# which could answer with another account's credentials.
git -C "$tap" config --local credential.https://github.com.helper ''
# shellcheck disable=SC2016 # expanded by git's shell when it asks
git -C "$tap" config --local --add credential.https://github.com.helper \
    '!f() { test "$1" = get && printf "username=x-access-token\npassword=%s\n" "$(gh auth token)"; }; f'
git -C "$tap" commit --quiet -m "acs $version"
git -C "$tap" push --quiet origin HEAD
echo "tap: acs $version pushed to $repo"
