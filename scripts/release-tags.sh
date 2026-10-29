#!/bin/sh
# Every release tag this checkout has, newest first by version, one per line
# (docs/VERSIONING.md, "Tags"). Releases up to 0.18.0 are tagged vX.Y.Z and
# later ones X.Y.Z, so both forms are listed and ordered as one sequence:
# `git tag --sort=-v:refname` would compare the `v` as text and put every
# v-tag above every bare one.
#
#   scripts/release-tags.sh
#
# scripts/release-binaries.sh and scripts/release-changes.sh read the
# latest and the previous release from it.
set -eu
cd "$(dirname "$0")/.."

git tag --list 'v[0-9]*' '[0-9]*' |
    grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+$' |
    awk '{ v = $0; sub(/^v/, "", v); print v, $0 }' |
    sort -t. -k1,1nr -k2,2nr -k3,3nr |
    cut -d' ' -f2
