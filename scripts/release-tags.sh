#!/bin/sh
# Every release tag this checkout has, newest first by version, one per line
# (docs/VERSIONING.md, "Tags"). Releases up to 0.18.0 are tagged vX.Y.Z and
# later ones X.Y.Z, so both forms are listed and ordered as one sequence:
# `git tag --sort=-v:refname` would compare the `v` as text and put every
# v-tag above every bare one.
#
# One version can carry both spellings on the same commit: 0.19.0 is also
# published as v0.19.0, so installed acs up to 0.18.0 can upgrade to it
# (docs/VERSIONING.md, "Tags"). The bare one is listed first, always, not
# in whatever order `sort` leaves a tie.
#
#   scripts/release-tags.sh
#
# scripts/release-binaries.sh and scripts/release-changes.sh read the
# latest and the previous release from it.
set -eu
cd "$(dirname "$0")/.."

git tag --list 'v[0-9]*' '[0-9]*' |
    grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+$' |
    awk '{ v = $0; sub(/^v/, "", v); split(v, n, ".")
           print n[1], n[2], n[3], ($0 ~ /^v/), $0 }' |
    sort -k1,1nr -k2,2nr -k3,3nr -k4,4n |
    cut -d' ' -f5
