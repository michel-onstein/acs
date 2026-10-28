#!/bin/sh
# Everything that must pass before a change lands: format, lint, tests, docs.
set -eu
cd "$(dirname "$0")/.."

echo '== cargo fmt'
cargo fmt --all -- --check
echo '== cargo clippy'
cargo clippy --workspace --all-targets -- -D warnings
echo '== cargo clippy (linux)'
cargo clippy -p acs --all-targets --target x86_64-unknown-linux-musl -- -D warnings
echo '== cargo test'
# The tests that need a Linux container or the ssh host are #[ignore] here
# and run under scripts/test_linux.sh and scripts/e2e_ssh.sh: a green run on
# a Mac says nothing about them, so it says how many it left out (acs-hdj4).
log=$(mktemp)
trap 'rm -f "$log" "$log.failed"' EXIT
{ cargo test --workspace 2>&1 || touch "$log.failed"; } | tee "$log"
[ ! -e "$log.failed" ] || exit 1
ignored=$(sed -n 's/^test result: .* \([0-9][0-9]*\) ignored;.*/\1/p' "$log" |
    awk '{ n += $1 } END { print n + 0 }')
echo "== $ignored tests ignored: not run by this script," \
    'see scripts/test_linux.sh and scripts/e2e_ssh.sh'
echo '== shellcheck'
if command -v shellcheck >/dev/null; then
    shellcheck scripts/*.sh scripts/e2e/*.sh
else
    echo 'shellcheck is not installed: skipped'
fi
echo '== markdownlint'
npx -y markdownlint-cli2 '**/*.md' '#target' '#.claude'
echo '== ok'
