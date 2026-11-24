#!/bin/bash
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# After md1's install_hooks.sh. Point this clone's git hooks at scripts/git-hooks.
#
#   scripts/install-hooks.sh          # write core.hooksPath to the clone's config (idempotent)
#   scripts/install-hooks.sh --help   # this text
#
# The path is relative, so each worktree runs the hooks of its own checkout.
# Undo with: git config --local --unset core.hooksPath
set -euo pipefail

case "${1:-}" in
    -h | --help)
        sed -n '3,/^[^#]/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    "") ;;
    *)
        echo "usage: scripts/install-hooks.sh" >&2
        exit 64
        ;;
esac
git config --local core.hooksPath scripts/git-hooks
echo "install-hooks: core.hooksPath = scripts/git-hooks"
