#!/bin/sh
# Run the whole test suite on Linux (musl, in a container), then the
# multi-user isolation tests as root with two real users (DESIGN §4.5), then
# the network watcher against the real kernel (acs-4i2).
#
#   scripts/test_linux.sh [--platform linux/amd64] [--only NAME [-- ARGS...]]
#
# --only NAME runs one test target instead of all three steps: NAME is a
# file under tests/ without `.rs` (`netns`, `multiuser`, `client`, ...), or
# `lib` for the unit tests. It is the same container, volumes and flags as the
# full run — `multiuser` and `netns` still get the environment and
# `--include-ignored` their steps use — so a result from it is a result from
# the real thing (acs-pjk). ARGS after `--` go to the test harness: a name
# filter, `--nocapture`, `--exact`. There is no image to build, and the
# volumes are reused either way, so --only saves the rest of the suite and
# nothing else.
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

usage() {
    echo "usage: scripts/test_linux.sh [--platform P] [--only NAME [-- ARGS...]]" >&2
    exit 2
}

platform=
only=
while [ $# -gt 0 ]; do
    case $1 in
        --platform)
            [ $# -ge 2 ] || usage
            platform="--platform $2"
            shift 2
            ;;
        --only)
            [ $# -ge 2 ] || usage
            only=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
        *)
            echo "test_linux.sh: unknown argument: $1" >&2
            usage
            ;;
    esac
done
# Checked here rather than left to cargo, so a typo costs nothing and never
# reaches the container.
if [ -n "$only" ] && [ "$only" != lib ]; then
    case $only in
        *[!a-z0-9_]*)
            echo "test_linux.sh: --only takes a test name, not $only" >&2
            exit 2
            ;;
    esac
    if [ ! -f "tests/$only.rs" ]; then
        echo "test_linux.sh: no tests/$only.rs (or lib, for the unit tests)" >&2
        exit 2
    fi
fi
# Three steps with different flags have no one harness to hand them to.
if [ $# -gt 0 ] && [ -z "$only" ]; then
    echo "test_linux.sh: arguments after -- need --only" >&2
    exit 2
fi

# shellcheck disable=SC2086
exec docker run --rm $platform \
    --cap-add NET_ADMIN \
    -v "$root:/src:ro" \
    -v acs-cargo-registry:/usr/local/cargo/registry \
    -v "acs-linux-target-$id:/target" \
    -e CARGO_TARGET_DIR=/target \
    -e "ACS_LINUX_ONLY=$only" \
    -w /src \
    rust:alpine sh -euc '
        # musl-dev to link; ssh-keygen because the release signature is made
        # and checked with it (acs-o9v), so src/signature.rs and every
        # upgrade and update-check test needs it on PATH. git because the
        # release-notes tests in tests/scripts.rs build a history to read.
        apk add --no-cache musl-dev openssh-keygen git >/dev/null
        adduser -D alice 2>/dev/null || true
        adduser -D bob 2>/dev/null || true
        # Test binaries and the acs they exec must be reachable by alice/bob.
        chmod 755 /target
        # The isolation tests are #[ignore] outside this step (acs-hdj4);
        # peer_helper is the child half of one of them, run by its parent.
        multiuser() {
            ACS_MULTIUSER_TEST=1 cargo test -p acs --test multiuser -- \
                --include-ignored --skip peer_helper --test-threads=1 "$@"
        }
        netns() {
            ACS_NETNS_TEST=1 cargo test -p acs --test netns -- \
                --include-ignored --test-threads=1 --nocapture "$@"
        }
        case $ACS_LINUX_ONLY in
            "")
                echo "== cargo test (root)"
                cargo test -p acs
                echo "== multi-user isolation"
                multiuser
                # A step of its own: it adds and removes interfaces, which
                # moves what getifaddrs reports process-wide, and netwatch
                # and netmatch have tests that read that while asserting it
                # holds still.
                echo "== the kernel drives the network watcher"
                netns
                ;;
            multiuser)
                echo "== multi-user isolation"
                multiuser "$@"
                ;;
            netns)
                echo "== the kernel drives the network watcher"
                netns "$@"
                ;;
            lib)
                echo "== cargo test --lib (root)"
                cargo test -p acs --lib -- "$@"
                ;;
            *)
                echo "== cargo test --test $ACS_LINUX_ONLY (root)"
                cargo test -p acs --test "$ACS_LINUX_ONLY" -- "$@"
                ;;
        esac
        echo "== ok"
    ' sh "$@"
