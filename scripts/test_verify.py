#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
"""Tests of the completion line of scripts/verify.sh (acs-7v9i).

verify.sh ends with exactly one line, `verify: PASS`, `verify: FAIL (<step>)` or `verify: FAIL (interrupted)`, so a
caller can wait on that line and never on a guess at the last step's output. The tests run verify.sh with
ACS_VERIFY_STEPS naming a step list of their own in place of the real steps.

    python3 scripts/test_verify.py
"""

import os
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERIFY = ROOT / "scripts" / "verify.sh"


class CompletionLineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def steps(self, body):
        path = Path(self.tmp.name) / "steps.sh"
        path.write_text(body)
        env = dict(os.environ, ACS_VERIFY_STEPS=str(path), ACS_QUIET="0")
        return env

    def run_verify(self, body):
        return subprocess.run(
            ["sh", str(VERIFY)], env=self.steps(body), capture_output=True, text=True, timeout=60, check=False
        )

    def test_pass_ends_with_pass_line(self):
        result = self.run_verify("step one true\nstep two true\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[-1], "verify: PASS")
        self.assertEqual(result.stdout.count("verify: "), 1)

    def test_failing_step_names_the_step(self):
        result = self.run_verify("step one true\nstep 'the broken one' false\nstep three true\n")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout.splitlines()[-1], "verify: FAIL (the broken one)")
        self.assertEqual(result.stdout.count("verify: "), 1)

    def test_failing_step_keeps_its_exit_status(self):
        result = self.run_verify("step broken sh -c 'exit 7'\n")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout.splitlines()[-1], "verify: FAIL (broken)")

    def test_interrupt_ends_with_interrupted_line(self):
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=sig.name):
                proc = subprocess.Popen(
                    ["sh", str(VERIFY)],
                    env=self.steps("step slow sleep 30\n"),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,  # quiet.sh prints a traceback when interrupted
                    text=True,
                    start_new_session=True,
                )
                time.sleep(1)
                # The whole group, as Ctrl-C does: the running step dies and the trap runs.
                os.killpg(proc.pid, sig)
                out, _ = proc.communicate(timeout=30)
                self.assertNotEqual(proc.returncode, 0)
                self.assertEqual(out.splitlines()[-1], "verify: FAIL (interrupted)")
                self.assertEqual(out.count("verify: "), 1)


if __name__ == "__main__":
    unittest.main()
