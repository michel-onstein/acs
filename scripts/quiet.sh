#!/bin/bash
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
# Run a command; one line when it passes, the failure's excerpt when it fails. scripts/lib/quiet.py holds the usage,
# the exit statuses and the logic.
here="$(cd "$(dirname "$0")" && pwd)"
exec python3 "$here/lib/quiet.py" "$@"
