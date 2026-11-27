#!/bin/sh
# Everything that must pass before a change lands: format, lint, tests, docs.
#
# Each step runs through scripts/quiet.sh: one `ok:` line when it passes, and when it fails the lines that explain
# it, with the whole output kept in a log under build/quiet/ either way. ACS_QUIET=0 streams everything instead.
#
# The last line of output, on every way out, is exactly one of (acs-7v9i):
#   verify: PASS
#   verify: FAIL (<step>)        the step that failed, by its label; the exit status is that step's
#   verify: FAIL (interrupted)   SIGINT or SIGTERM; the exit status is 130 or 143
# A caller waits on the process; one that must poll a log polls for `^verify: (PASS|FAIL)`, never for a guess at
# what the last step prints. ACS_VERIFY_STEPS=<file> runs that file (lines of `step LABEL COMMAND...`) in place of
# the real steps; it exists for scripts/test_verify.py.
set -eu
cd "$(dirname "$0")/.."

current=startup
finished=
log=
finish() {
    status=$?
    trap - EXIT
    [ -z "$log" ] || rm -f "$log"
    if [ -n "$finished" ]; then
        echo 'verify: PASS'
    else
        echo "verify: FAIL ($current)"
    fi
    exit "$status"
}
trap finish EXIT
trap 'current=interrupted; exit 130' INT
trap 'current=interrupted; exit 143' TERM

step() {
    label=$1
    current=$label
    shift
    scripts/quiet.sh --label "$label" -- "$@"
}

real_steps() {
    step 'cargo fmt' cargo fmt --all -- --check
    step 'cargo clippy' cargo clippy --workspace --all-targets -- -D warnings
    step 'cargo clippy (linux)' cargo clippy -p acs --all-targets --target x86_64-unknown-linux-musl -- -D warnings
    # The tests that need a Linux container or the ssh host are #[ignore] here
    # and run under scripts/test_linux.sh and scripts/e2e_ssh.sh: a green run on
    # a Mac says nothing about them, so it says how many it left out (acs-hdj4).
    log=$(mktemp)
    current='cargo test'
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
    current=markdownlint
    scripts/markdownlint.sh
}

if [ -n "${ACS_VERIFY_STEPS:-}" ]; then
    # shellcheck disable=SC1090
    . "$ACS_VERIFY_STEPS"
else
    real_steps
fi
echo '== ok'
finished=1
