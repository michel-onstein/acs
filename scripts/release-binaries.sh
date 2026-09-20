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

# Sign the checksums (acs-o9v). They and the archives come from the same
# place, so unsigned they prove only that an archive matches what that place
# said it should be; acs and install.sh check this signature against a key
# they carry before believing either. No key, no release: a release without
# a signature is one every client refuses.
# ACS_SIGNING_KEY may name either half of the pair. The private half makes
# ssh-keygen prompt for its passphrase, which needs a terminal; naming the
# public half signs through ssh-agent instead, so a passphrase-protected
# key can be used unattended once it is loaded
# (ssh-add --apple-use-keychain ~/.ssh/acs-release).
key=${ACS_SIGNING_KEY:-$HOME/.ssh/acs-release}
pub=${key%.pub}.pub
[ -f "$key" ] || {
    echo "no release signing key at $key (set ACS_SIGNING_KEY)" >&2
    exit 1
}
[ -f "$pub" ] || {
    echo "no public half at $pub to check the signature against" >&2
    exit 1
}
# Nothing chosen by hand: sign through ssh-agent when it holds this key.
# ssh-keygen reads a passphrase from /dev/tty, so signing with the private
# half of a protected key needs a terminal and a release run without one
# would stall on the prompt (or, with no askpass, fail). Naming the public
# half makes ssh-keygen ask the agent instead, which needs nothing.
if [ -z "${ACS_SIGNING_KEY:-}" ]; then
    want=$(ssh-keygen -lf "$pub" | awk '{print $2}')
    if ssh-add -l 2>/dev/null | awk '{print $2}' | grep -qxF "$want"; then
        key=$pub
        echo "== signing key is in ssh-agent"
    fi
fi
echo "== signing SHA256SUMS"
ssh-keygen -Y sign -q -n acs-release -f "$key" "$work/assets/SHA256SUMS"
printf 'releases@acs namespaces="acs-release" %s\n' "$(cat "$pub")" > "$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I releases@acs -n acs-release \
    -s "$work/assets/SHA256SUMS.sig" < "$work/assets/SHA256SUMS" >/dev/null || {
    echo "the signature just made does not verify; not publishing" >&2
    exit 1
}

if [ "$dry" = 1 ]; then
    echo "dry run: assets in $work/assets"
    ls -l "$work/assets"
    [ -n "${ACS_NO_TAP:-}" ] || scripts/update-tap.sh "$work/acs.rb" --dry-run
    exit 0
fi

echo "== publishing $tag"
if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" \
        "$work/assets/SHA256SUMS.sig" "$work/assets/install.sh" --clobber
    gh release edit "$tag" --notes-file "$work/assets/NOTES.md"
else
    gh release create "$tag" "$work"/assets/*.tar.gz "$work/assets/SHA256SUMS" \
        "$work/assets/SHA256SUMS.sig" "$work/assets/install.sh" \
        --verify-tag --title "acs $version" --notes-file "$work/assets/NOTES.md"
fi
gh release view "$tag" --json url -q .url

[ -z "${ACS_NO_TAP:-}" ] || exit 0
echo "== updating the Homebrew tap"
scripts/update-tap.sh "$work/acs.rb" || {
    echo "the release is published but the tap is not: retry with scripts/update-tap.sh $tag" >&2
    exit 1
}
