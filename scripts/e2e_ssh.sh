#!/bin/sh
# End-to-end checks over real ssh against a container host (DESIGN §9.1):
# first-contact install from the macOS complete build, TUI escape sequences,
# the kitty keyboard protocol, mouse reports, OSC 52, key selection by -i and
# by identity_file, drops (a killed connection, a frozen host), the Ctrl-L
# sent after a re-attach and a resume, and a -L forward (the session's ssh
# alone binds the port).
#
#   scripts/e2e_ssh.sh [--no-build [--allow-stale-dist]]
#
# Needs docker and `cargo xtask dist` output (built unless --no-build).
# --no-build reuses dist/ only when `dist/source.stamp` says it was built from
# this source tree (acs-gb4); --allow-stale-dist reuses it regardless.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
build=1
allow_stale=0
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        --allow-stale-dist) allow_stale=1 ;;
        *)
            echo "usage: $0 [--no-build [--allow-stale-dist]]" >&2
            exit 2
            ;;
    esac
done
# Per checkout, so worktrees running this at once keep their own host.
name=acs-e2e-host-$(printf %s "$root" | cksum | cut -d' ' -f1)
port=${ACS_E2E_PORT:-}
work=$(mktemp -d /tmp/acs-e2e.XXXXXX)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

if [ "$build" = 1 ]; then
    cargo xtask dist
elif [ "$allow_stale" = 1 ]; then
    echo "e2e: --allow-stale-dist: reusing dist/ unchecked" >&2
else
    # dist/ is only worth reusing if it was built from what is on disk now:
    # a red run from a binary two edits old reads exactly like a real one
    # (acs-gb4). The stamp is written by `cargo xtask dist` into the
    # directory it wipes and refills, so it cannot outlive those binaries.
    want=$(scripts/source_stamp.sh)
    have=$(cat dist/source.stamp 2>/dev/null || echo '<none>')
    if [ "$want" != "$have" ]; then
        echo >&2
        echo "e2e: REFUSING --no-build: dist/ was not built from this tree." >&2
        echo "  dist/source.stamp  $have" >&2
        echo "  this source tree   $want" >&2
        echo "A failure from a stale binary is indistinguishable from a real" >&2
        echo "one. Rebuild by dropping --no-build, or pass" >&2
        echo "--allow-stale-dist to reuse dist/ anyway." >&2
        exit 1
    fi
fi
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) client=dist/aarch64-apple-darwin/acs ;;
    Darwin-x86_64) client=dist/x86_64-apple-darwin/acs ;;
    Linux-x86_64) client=dist/x86_64-unknown-linux-musl/acs ;;
    Linux-aarch64) client=dist/aarch64-unknown-linux-musl/acs ;;
    *) echo "unsupported local platform" >&2; exit 1 ;;
esac

ssh-keygen -q -t ed25519 -N '' -f "$work/key"
docker build -q -t acs-e2e-host scripts/e2e >/dev/null
docker rm -f "$name" >/dev/null 2>&1 || true
# Without ACS_E2E_PORT docker picks a free port.
docker run -d --name "$name" -p "127.0.0.1:$port:22" \
    -e AUTHORIZED_KEY="$(cat "$work/key.pub")" acs-e2e-host >/dev/null
port=$(docker port "$name" 22/tcp | head -n 1 | sed 's/.*://')
# Wait for sshd.
i=0
until ssh -F /dev/null -i "$work/key" -p "$port" -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=2 \
    dev@127.0.0.1 true 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || { echo "sshd did not come up" >&2; exit 1; }
    sleep 0.2
done

export ACS_E2E_CLIENT="$root/$client"
export ACS_E2E_KEY="$work/key"
export ACS_E2E_PORT="$port"
export ACS_E2E_CONTAINER="$name"
cargo test -p acs --test e2e_ssh -- --test-threads=1 --nocapture
