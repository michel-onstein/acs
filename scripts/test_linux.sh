#!/bin/sh
# Run the whole test suite on Linux (musl, in a container), then the
# multi-user isolation tests as root with two real users (DESIGN §4.5), then
# the network watcher against the real kernel (acs-4i2).
#
#   scripts/test_linux.sh [--platform linux/amd64]
#
# Needs docker (or podman as docker). Cargo's registry and the target
# directory live in named volumes so later runs are fast. The target volume
# is per checkout: every checkout mounts at /src, so in a shared one cargo
# cannot tell one worktree's sources from another's and runs a stale build.
#
# CAP_NET_ADMIN is for tests/netns.rs alone: a container has a network
# namespace of its own, so with that capability an interface can genuinely
# be created, addressed and torn down inside it, which is what makes the
# kernel emit the NETLINK_ROUTE messages netwatch.rs subscribes to. Nothing
# reaches the host's network — the namespace is the isolation — and no
# other step uses it.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
id=$(printf %s "$root" | cksum | cut -d' ' -f1)
platform=
if [ "${1:-}" = "--platform" ]; then
    platform="--platform $2"
fi

# shellcheck disable=SC2086
exec docker run --rm $platform \
    --cap-add NET_ADMIN \
    -v "$root:/src:ro" \
    -v acs-cargo-registry:/usr/local/cargo/registry \
    -v "acs-linux-target-$id:/target" \
    -e CARGO_TARGET_DIR=/target \
    -w /src \
    rust:alpine sh -euc '
        # musl-dev to link; ssh-keygen because the release signature is made
        # and checked with it (acs-o9v), so src/signature.rs and every
        # upgrade and update-check test needs it on PATH.
        apk add --no-cache musl-dev openssh-keygen >/dev/null
        adduser -D alice 2>/dev/null || true
        adduser -D bob 2>/dev/null || true
        # Test binaries and the acs they exec must be reachable by alice/bob.
        chmod 755 /target
        echo "== cargo test (root)"
        cargo test -p acs
        echo "== multi-user isolation"
        ACS_MULTIUSER_TEST=1 cargo test -p acs --test multiuser -- --test-threads=1
        # A step of its own: it adds and removes interfaces, which moves
        # what getifaddrs reports process-wide, and netwatch and netmatch
        # have tests that read that while asserting it holds still.
        echo "== the kernel drives the network watcher"
        ACS_NETNS_TEST=1 cargo test -p acs --test netns -- --test-threads=1 --nocapture
        echo "== ok"
    '
