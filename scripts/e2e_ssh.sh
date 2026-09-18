#!/bin/sh
# End-to-end checks over real ssh against a container host (DESIGN §9.1):
# first-contact install from the macOS complete build, TUI escape sequences,
# the kitty keyboard protocol, mouse reports, OSC 52, -i key selection, and
# drops (a killed connection, a frozen host).
#
#   scripts/e2e_ssh.sh [--no-build]
#
# Needs docker and `cargo xtask dist` output (built unless --no-build).
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
name=acs-e2e-host
port=${ACS_E2E_PORT:-2222}
work=$(mktemp -d /tmp/acs-e2e.XXXXXX)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

if [ "${1:-}" != "--no-build" ]; then
    cargo xtask dist
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
docker run -d --name "$name" -p "127.0.0.1:$port:22" \
    -e AUTHORIZED_KEY="$(cat "$work/key.pub")" acs-e2e-host >/dev/null
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
