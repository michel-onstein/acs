#!/bin/sh
# Everything that must pass before a change lands: format, lint, tests, docs.
#
# Each step runs through scripts/quiet.sh: one `ok:` line when it passes, and when it fails the lines that explain
# it, with the whole output kept in a log under build/quiet/ either way. ACS_QUIET=0 streams everything instead.
set -eu
cd "$(dirname "$0")/.."

step() {
    label=$1
    shift
    scripts/quiet.sh --label "$label" -- "$@"
}

step 'cargo fmt' cargo fmt --all -- --check
step 'cargo clippy' cargo clippy --workspace --all-targets -- -D warnings
step 'cargo clippy (linux)' cargo clippy -p acs --all-targets --target x86_64-unknown-linux-musl -- -D warnings
# The tests that need a Linux container or the ssh host are #[ignore] here
# and run under scripts/test_linux.sh and scripts/e2e_ssh.sh: a green run on
# a Mac says nothing about them, so it says how many it left out (acs-hdj4).
log=$(mktemp)
trap 'rm -f "$log"' EXIT
scripts/quiet.sh --label 'cargo test' --log "$log" -- cargo test --workspace
ignored=$(sed -n 's/^test result: .* \([0-9][0-9]*\) ignored;.*/\1/p' "$log" |
    awk '{ n += $1 } END { print n + 0 }')
echo "== $ignored tests ignored: not run by this script," \
    'see scripts/test_linux.sh and scripts/e2e_ssh.sh'
# The size budget (acs-i3vj): both Linux slim release builds and the payload
# blob, measured as `cargo xtask dist` measures them, so a change that takes
# x86_64 Linux over budget fails here and not after a release is tagged.
# About 25 s after a source change, 2 s with nothing changed;
# ACS_SKIP_SIZE=1 skips it. `scripts/version-bump.sh` runs the same check on
# the tree it tags, and that one cannot be skipped (docs/VERSIONING.md).
if [ -n "${ACS_SKIP_SIZE:-}" ]; then
    echo '== size budget: skipped (ACS_SKIP_SIZE); the version bump still checks it'
else
    step 'size budget' cargo xtask size
fi
if command -v shellcheck >/dev/null; then
    step 'shellcheck' shellcheck -x scripts/*.sh scripts/e2e/*.sh scripts/git-hooks/pre-commit
else
    echo '== shellcheck is not installed: skipped'
fi
for suite in scripts/test_*.py; do
    step "$(basename "$suite" .py)" python3 "$suite"
done
step 'project logs' python3 scripts/check_project_logs.py
scripts/markdownlint.sh
echo '== ok'
