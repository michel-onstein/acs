#!/bin/sh
# Prints a fingerprint of the sources `cargo xtask dist` builds the binaries
# from (acs-gb4). `cargo xtask dist` writes it to `dist/source.stamp` once the
# build has succeeded; `scripts/e2e_ssh.sh --no-build` recomputes it and
# refuses to reuse a `dist/` built from anything else.
#
# It hashes file contents, not a git revision: the failure it has to catch is
# an *uncommitted* edit — build dist/, edit a source file, run --no-build —
# where HEAD is unchanged and a "dirty" flag can be set both times.
#
# `tests/` is deliberately not in here. Those are compiled by `cargo test` on
# every run, so they are never stale, and iterating on an e2e case is what
# --no-build is for: editing one must not invalidate dist/.
set -eu
cd "$(dirname "$0")/.."

if command -v sha256sum >/dev/null 2>&1; then
    set -- sha256sum
elif command -v shasum >/dev/null 2>&1; then
    set -- shasum -a 256
else
    echo "source_stamp.sh: neither sha256sum nor shasum is installed" >&2
    exit 1
fi

# Everything that ends up in the binaries, plus the code that packages them.
files=$(
    for p in Cargo.toml Cargo.lock build.rs src xtask/Cargo.toml xtask/src; do
        [ -e "$p" ] && find "$p" -type f -print
    done | LC_ALL=C sort
)
if [ -z "$files" ]; then
    echo "source_stamp.sh: no sources under $(pwd)" >&2
    exit 1
fi

# One "<hash>  <name>" line per file, hashed in turn: an edit, a rename, an
# added file and a removed file each move the fingerprint.
printf '%s\n' "$files" | tr '\n' '\0' | xargs -0 "$@" | "$@" | cut -d' ' -f1
