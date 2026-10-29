#!/bin/sh
# Update the Homebrew tap (docs/VERSIONING.md, "Homebrew"): commit a rendered
# Formula/acs.rb to michel-onstein/homebrew-acs and push it.
#
#   scripts/update-tap.sh FORMULA [--dry-run]    a file from cargo xtask formula
#   scripts/update-tap.sh TAG [--dry-run]        render that release's formula
#
# TAG is the release's tag as published: vX.Y.Z up to v0.18.0, X.Y.Z from
# 0.19.0 on (docs/VERSIONING.md, "Tags").
#
# scripts/release-binaries.sh runs it after publishing a release; run it by
# hand to retry. The tap is cloned into a temporary directory; a formula that
# is unchanged, or older than the tap's, is left alone. --dry-run shows the
# change and pushes nothing. ACS_TAP_REPO names another tap repository (the
# tests use a local one). Pushing to GitHub uses gh's token (GH_TOKEN works).
#
# A TAG argument's SHA256SUMS is fetched from the build's release channel
# (`cargo xtask channel`), or from ACS_RELEASES_URL, and its SHA256SUMS.sig
# is checked against that channel's release key before anything is rendered
# (acs-oxl4): Homebrew users never see the signature, so an unchecked file
# here would launder a tampered release into the formula's pinned sha256s.
# The key is the build's, as in the packaged installer and the formula, so a
# fork built with ACS_DEFAULT_RELEASE_KEY trusts its own; there is no
# variable that names another. ACS_XTASK runs that xtask binary instead of
# `cargo xtask` (the tests use the one cargo built).
set -eu

usage() {
    echo "usage: scripts/update-tap.sh FORMULA|TAG [--dry-run]" >&2
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
root=$(cd "$(dirname "$0")/.." && pwd)

xtask() {
    if [ -n "${ACS_XTASK:-}" ]; then
        "$ACS_XTASK" "$@"
    else
        (cd "$root" && cargo xtask "$@")
    fi
}

work=$(mktemp -d "${TMPDIR:-/tmp}/acs-tap.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$arg" in
    */* | *.rb) formula=$arg ;;
    v[0-9]* | [0-9]*)
        # A release tag, either form: render its formula from the published SHA256SUMS, once
        # its signature checks out against the build's release key.
        xtask channel > "$work/channel"
        key=$(sed -n 's/^key //p' "$work/channel" | head -n 1)
        releases=${ACS_RELEASES_URL:-$(sed -n 's/^releases //p' "$work/channel" | head -n 1)}
        if [ -z "$key" ] || [ -z "$releases" ]; then
            echo "cargo xtask channel named no release key or releases URL" >&2
            exit 1
        fi
        command -v ssh-keygen >/dev/null 2>&1 || {
            echo "need ssh-keygen to check the release signature" >&2
            exit 1
        }
        base=$releases/download/$arg
        curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" || {
            echo "cannot download $base/SHA256SUMS" >&2
            exit 1
        }
        curl -fsSL -o "$work/SHA256SUMS.sig" "$base/SHA256SUMS.sig" || {
            echo "the release has no signature for SHA256SUMS ($base/SHA256SUMS.sig); the tap is left alone" >&2
            exit 1
        }
        printf 'releases@acs namespaces="acs-release" %s\n' "$key" > "$work/allowed_signers"
        ssh-keygen -Y verify -f "$work/allowed_signers" -I releases@acs -n acs-release \
            -s "$work/SHA256SUMS.sig" < "$work/SHA256SUMS" >/dev/null 2>&1 || {
            echo "$base/SHA256SUMS is not signed by this build's release key ($key); the tap is left alone" >&2
            echo "(a fork builds xtask with its ACS_DEFAULT_RELEASE_KEY; docs/VERSIONING.md, \"Forking\")" >&2
            exit 1
        }
        xtask formula --version "${arg#v}" --sums "$work/SHA256SUMS" --out "$work/acs.rb"
        formula=$work/acs.rb
        ;;
    *) formula=$arg ;;
esac
[ -f "$formula" ] || { echo "no formula at $formula" >&2; exit 1; }

# The release a formula installs, from the line `cargo xtask formula` writes
# for it — not from its download URLs, whose shape is whatever a fork's
# releases URL makes it (acs-u3qn). A tap formula from before that line
# existed reads as no version, and is replaced.
version_of() {
    sed -n 's|^# acs-version: \([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$|\1|p' "$1" | head -n 1
}
version=$(version_of "$formula")
[ -n "$version" ] || {
    echo "$formula names no release (no '# acs-version: X.Y.Z' line; render it with cargo xtask formula)" >&2
    exit 1
}

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
