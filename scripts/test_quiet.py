#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
"""Tests of scripts/quiet.sh and scripts/lib/quiet.py: one line on a pass, the failure's excerpt on a failure.

    python3 scripts/test_quiet.py
"""

import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts" / "lib"))

import quiet  # noqa: E402

PASSING = [f"test module::case_{i} ... ok" for i in range(400)]

CARGO_TEST_FAILURE = [
    "   Compiling acs-core v0.1.0",
    "    Finished `test` profile",
    "     Running unittests src/lib.rs",
    "",
    "running 401 tests",
    *PASSING[:200],
    "test module::broken ... FAILED",
    *PASSING[200:],
    "",
    "failures:",
    "",
    "---- module::broken stdout ----",
    "thread 'module::broken' panicked at crates/acs-core/src/lib.rs:10:5:",
    "assertion `left == right` failed",
    "",
    "failures:",
    "    module::broken",
    "",
    "test result: FAILED. 400 passed; 1 failed; 0 ignored",
    "",
    "error: test failed, to rerun pass `-p acs-core --lib`",
]

RUSTC_ERROR = [
    "   Compiling acs-api v0.1.0",
    *[f"   Compiling dep{i} v1.0.0" for i in range(300)],
    "error[E0308]: mismatched types",
    "  --> crates/acs-api/src/lib.rs:3:5",
    "   |",
    "3  |     1u8",
    "   |     ^^^ expected `String`, found `u8`",
    "",
    "For more information about this error, try `rustc --explain E0308`.",
    "error: could not compile `acs-api` (lib) due to 1 previous error",
]

UNITTEST_FAILURE = [
    *[f"test_case_{i} (test_x.Tests.test_case_{i}) ... ok" for i in range(200)],
    "",
    "======================================================================",
    "FAIL: test_broken (test_x.Tests.test_broken)",
    "----------------------------------------------------------------------",
    "Traceback (most recent call last):",
    '  File "test_x.py", line 9, in test_broken',
    "    self.assertEqual(1, 2)",
    "AssertionError: 1 != 2",
    "",
    "----------------------------------------------------------------------",
    "Ran 201 tests in 0.1s",
    "",
    "FAILED (failures=1)",
]

XCODEBUILD_FAILURE = [
    *[f"CompileSwift normal arm64 /src/File{i}.swift" for i in range(500)],
    "/src/PL1Tests/HomeTests.swift:42:9: error: Expectation failed: (model.items.count → 0) == 3",
    "✘ Test \"home shows items\" recorded an issue at HomeTests.swift:42:9",
    *[f"✔ Test \"case {i}\" passed after 0.001 seconds." for i in range(100)],
    "** TEST FAILED **",
]

LATER_NOISE = [f"ℹ summary line {i}" for i in range(60)]

# node:test's spec reporter, as node 22 prints it: failures marked U+2716, no `not ok`.
NODE_SPEC_FAILURES = [
    *[f"✔ case {i} (0.1ms)" for i in range(50)],
    "✖ first broken (1.0ms)",
    "✔ middle (0.1ms)",
    "✖ second broken (1.0ms)",
    *LATER_NOISE,
]

# tsc's plain output: path(line,col): error TSnnnn: message.
TSC_ERRORS = [
    "src/a.ts(10,3): error TS2345: Argument of type 'number' is not assignable to parameter of type 'string'.",
    "src/b.ts(4,7): error TS2322: Type 'string' is not assignable to type 'number'.",
    *LATER_NOISE,
]

# cargo deny: the code group is a word, not rustc's letter and digits.
CARGO_DENY = [
    "error[vulnerability]: Uncaught panic in 'x'",
    "  ┌─ /src/Cargo.lock:10:1",
    "  │",
    "10 │ x 1.0.0",
    "  │ ------- security vulnerability detected",
    "",
    *LATER_NOISE,
]


class ExcerptTests(unittest.TestCase):
    def test_node_spec_failures_are_all_kept(self):
        text = "\n".join(quiet.excerpt(NODE_SPEC_FAILURES))
        self.assertIn("✖ first broken", text)
        self.assertIn("✖ second broken", text)
        self.assertNotIn("✔ case 0", text)

    def test_every_tsc_error_is_kept(self):
        text = "\n".join(quiet.excerpt(TSC_ERRORS))
        self.assertIn("src/a.ts(10,3): error TS2345", text)
        self.assertIn("src/b.ts(4,7): error TS2322", text)

    def test_a_cargo_deny_block_is_kept_to_its_blank_line(self):
        text = "\n".join(quiet.excerpt(CARGO_DENY))
        self.assertIn("error[vulnerability]: Uncaught panic in 'x'", text)
        self.assertIn("security vulnerability detected", text)

    def test_cargo_test_keeps_the_failure_report_and_drops_the_passes(self):
        out = quiet.excerpt(CARGO_TEST_FAILURE)
        text = "\n".join(out)
        self.assertIn("test module::broken ... FAILED", text)
        self.assertIn("panicked at crates/acs-core/src/lib.rs:10:5", text)
        self.assertIn("test result: FAILED. 400 passed; 1 failed", text)
        self.assertIn("error: test failed, to rerun pass `-p acs-core --lib`", text)
        self.assertNotIn("case_0 ... ok", text)
        self.assertLess(len(out), 40)

    def test_a_rustc_diagnostic_is_kept_to_its_blank_line(self):
        text = "\n".join(quiet.excerpt(RUSTC_ERROR))
        self.assertIn("error[E0308]: mismatched types\n  --> crates/acs-api/src/lib.rs:3:5", text)
        self.assertIn("expected `String`, found `u8`", text)
        self.assertNotIn("Compiling dep", text)

    def test_unittest_keeps_the_failure_block_and_its_verdict(self):
        text = "\n".join(quiet.excerpt(UNITTEST_FAILURE))
        self.assertIn("FAIL: test_broken", text)
        self.assertIn("AssertionError: 1 != 2", text)
        self.assertIn("FAILED (failures=1)", text)
        self.assertNotIn("test_case_0 ", text)

    def test_xcodebuild_keeps_the_error_and_the_failed_test(self):
        text = "\n".join(quiet.excerpt(XCODEBUILD_FAILURE))
        self.assertIn("HomeTests.swift:42:9: error: Expectation failed", text)
        self.assertIn('✘ Test "home shows items" recorded an issue', text)
        self.assertIn("** TEST FAILED **", text)
        self.assertNotIn("✔ Test", text)
        self.assertNotIn("CompileSwift normal arm64 /src/File0.swift", text)

    def test_with_nothing_recognised_the_tail_is_shown(self):
        lines = [f"line {i}" for i in range(100)]
        out = quiet.excerpt(lines, tail=5)
        self.assertEqual(out, ["…", "line 95", "line 96", "line 97", "line 98", "line 99"])

    def test_a_short_log_is_shown_whole(self):
        self.assertEqual(quiet.excerpt(["one", "two"]), ["one", "two"])

    def test_the_limit_caps_the_blocks_and_says_how_many_were_left_out(self):
        lines = [f"error: problem {i}" for i in range(500)] + ["done"]
        out = quiet.excerpt(lines, limit=50, tail=10)
        self.assertLessEqual(len(out), 50 + 5)
        self.assertRegex(out[-1], r"^… \d+ more matching lines left out \(the limit is 50\)$")
        self.assertIn("done", out)

    def test_gaps_are_marked_and_a_match_keeps_the_line_before(self):
        lines = ["a", "b", "test t ... FAILED", "c", "d", "e", "f"]
        self.assertEqual(quiet.excerpt(lines, tail=1), ["…", "b", "test t ... FAILED", "…", "f"])

    def test_a_diagnostic_with_no_blank_line_after_it_is_capped(self):
        lines = ["error: no blank follows", *[f"x{i}" for i in range(500)]]
        out = quiet.excerpt(lines, tail=0)
        self.assertEqual(out[0], "error: no blank follows")
        self.assertEqual(out, ["error: no blank follows", *[f"x{i}" for i in range(quiet.BLOCK_MAX)]])

    def test_a_failure_block_on_the_last_line_does_not_overrun(self):
        self.assertEqual(quiet.excerpt(["x", "FAIL: last"], tail=0), ["…", "FAIL: last"])


class MainTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="quiet-test-")
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.env = {"ACS_QUIET_DIR": self.dir}

    def main(self, *argv, env=None):
        out = io.StringIO()
        code = quiet.main(list(argv), env={**self.env, **(env or {})}, out=out)
        return code, out.getvalue()

    def test_a_pass_is_one_line_with_the_log(self):
        code, out = self.main("--label", "chatty", "--", "/bin/sh", "-c", "seq 1 1000")
        self.assertEqual(code, 0)
        self.assertRegex(out, r"^ok: chatty \(\d+\.\d s; log .*chatty\.log\)\n$")
        self.assertEqual(len(Path(self.dir, "chatty.log").read_text().splitlines()), 1000)

    def test_a_failure_prints_the_excerpt_and_keeps_its_status(self):
        script = "seq 1 1000; echo 'error: it broke' >&2; exit 3"
        code, out = self.main("--label", "broken", "--", "/bin/sh", "-c", script)
        self.assertEqual(code, 3)
        self.assertTrue(out.startswith("FAILED (exit 3): broken"))
        self.assertIn("error: it broke", out)
        self.assertNotIn("\n500\n", out)
        self.assertIn(f"full log (1001 lines): {Path(self.dir, 'broken.log')}", out)

    def test_checkout_root_is_this_checkout(self):
        self.assertEqual(quiet.CHECKOUT_ROOT, ROOT)

    def test_two_checkouts_with_the_same_label_do_not_share_a_file(self):
        """Regression for pl1-9pz7: the default log directory is keyed to the checkout (here, via quiet.py's own
        path), not to a shared /tmp directory, so two checkouts running the same label never truncate one
        another's log."""
        checkout_a = Path(tempfile.mkdtemp(prefix="quiet-checkout-a-"))
        checkout_b = Path(tempfile.mkdtemp(prefix="quiet-checkout-b-"))
        self.addCleanup(shutil.rmtree, checkout_a, ignore_errors=True)
        self.addCleanup(shutil.rmtree, checkout_b, ignore_errors=True)
        env = {key: value for key, value in os.environ.items() if key != "ACS_QUIET_DIR"}
        out_a, out_b = io.StringIO(), io.StringIO()
        code_a = quiet.main(["--label", "same", "--", "/bin/sh", "-c", "echo from-a"], env=env, out=out_a,
                             checkout_root=checkout_a)
        code_b = quiet.main(["--label", "same", "--", "/bin/sh", "-c", "echo from-b"], env=env, out=out_b,
                             checkout_root=checkout_b)
        self.assertEqual((code_a, code_b), (0, 0), out_a.getvalue() + out_b.getvalue())
        path_a = checkout_a / "build" / "quiet" / "same.log"
        path_b = checkout_b / "build" / "quiet" / "same.log"
        self.assertNotEqual(path_a, path_b)
        self.assertIn(str(path_a), out_a.getvalue())
        self.assertIn(str(path_b), out_b.getvalue())
        self.assertEqual(path_a.read_text(), "from-a\n")
        self.assertEqual(path_b.read_text(), "from-b\n")

    def test_the_label_defaults_to_the_command(self):
        code, out = self.main("--", "true")
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("ok: true ("))

    def test_log_names_the_file(self):
        log = Path(self.dir, "sub", "mine.log")
        code, _ = self.main("--log", str(log), "--", "/bin/sh", "-c", "echo hello")
        self.assertEqual(code, 0)
        self.assertEqual(log.read_text(), "hello\n")

    def test_a_missing_command_is_127(self):
        code, out = self.main("--", "/no/such/quiet-tool")
        self.assertEqual(code, 127)
        self.assertIn("cannot run /no/such/quiet-tool", out)

    def test_a_signal_is_128_plus_its_number(self):
        code, _ = self.main("--", "/bin/sh", "-c", "kill -TERM $$")
        self.assertEqual(code, 128 + 15)

    def test_quiet_0_passes_the_output_through(self):
        result = subprocess.run([str(ROOT / "scripts" / "quiet.sh"), "--", "/bin/sh", "-c", "echo through; exit 4"],
                                capture_output=True, text=True, env={**os.environ, "ACS_QUIET": "0"})
        self.assertEqual(result.returncode, 4)
        self.assertEqual(result.stdout, "through\n")

    def test_usage_errors_exit_64(self):
        for argv in ([], ["--"], ["--max", "x", "--", "true"], ["--nope", "--", "true"], ["--label"], ["true"]):
            with self.subTest(argv=argv):
                result = subprocess.run([str(ROOT / "scripts" / "quiet.sh"), *argv], capture_output=True, text=True)
                self.assertEqual(result.returncode, 64, result.stdout + result.stderr)

    def test_help(self):
        result = subprocess.run([str(ROOT / "scripts" / "quiet.sh"), "--help"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn("scripts/quiet.sh [--label NAME]", result.stdout)


if __name__ == "__main__":
    unittest.main()
