#!/usr/bin/env python3
# Copyright (c) 2026 Q-Jam B.V. All rights reserved.
"""Run a command and print one line when it passes, only what explains the failure when it fails.

    scripts/quiet.sh [--label NAME] [--log FILE] [--max N] [--tail N] -- <command> [<argument>...]

A passing `cargo test`, `xcodebuild test` or `npm run check` prints thousands of lines nobody reads, and an agent
pays for every one of them. This keeps the command's whole output, stdout and stderr together, in a log file and
prints

    ok: <label> (12.3 s; log <file>)

when it exits 0. When it fails it prints `FAILED (exit N): <label>`, then an excerpt, then the log's path:

- **the blocks that explain a failure**, found by `excerpt`: rustc's `error`/`warning` diagnostics to their blank
  line, cargo test's `failures:` section to its `test result:` line, a `test … FAILED` or `panicked at` line,
  unittest's `FAIL:`/`ERROR:` blocks and their tracebacks, XCTest's and Swift Testing's failed cases and issues,
  xcodebuild's `error:` lines and `** … FAILED **`, node:test's `not ok` and npm's `ERR!`, each with the line before;
- **the last --tail lines** (default 30) that are not progress or a pass (`Compiling …`, `test … ok`, `ok 3 …`,
  blank lines), which hold the summary and whatever the patterns missed;
- at most --max lines in all (default 200), the rest counted as omitted and left in the log.

The log is --log FILE, or `<ACS_QUIET_DIR, or build/quiet under this checkout>/<label>.log`, overwritten by the
next run of the same label in the same checkout, so the full output is one `sed -n` away when the excerpt is not
enough. Keying the default on the checkout rather than on a shared `/tmp` directory means two worktrees running the
same label at once never truncate one another's log out from under a still-reading failure excerpt. `ACS_QUIET=0`
streams the output unfiltered instead, as if this script were not there, and keeps the exit status.

Exit status: the command's own; 127 when it cannot be started; 64 on a usage error; 130 interrupted.
"""

import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

# This file lives at <checkout>/scripts/lib/quiet.py, so its own path names the checkout it runs from — one worktree
# per checkout, each with its own copy of this file at a different absolute path. Keying the default log directory
# on this, rather than on a shared /tmp directory, is what keeps two checkouts running the same label from writing
# one file between them (pl1-9pz7).
CHECKOUT_ROOT = Path(__file__).resolve().parent.parent.parent

DEFAULT_MAX = 200
DEFAULT_TAIL = 30
# A diagnostic block runs to its blank line, but never further than this: an `error:` line with no blank after it
# would otherwise take the rest of the log.
BLOCK_MAX = 40

# A line that opens a block running to the next blank line (rustc's diagnostics, with their source snippets).
BLOCK_TO_BLANK = re.compile(r"^(error|warning)(\[[\w-]+\])?: ")
# cargo test's failure report, from `failures:` to its `test result:` line.
CARGO_FAILURES = re.compile(r"^failures:\s*$")
CARGO_RESULT = re.compile(r"^test result: ")
# unittest's failure blocks, each closed by the next `====` or `----` rule.
UNITTEST_BLOCK = re.compile(r"^(FAIL|ERROR): ")
UNITTEST_RULE = re.compile(r"^(=|-){20,}\s*$")
# Lines that say something passed or is in progress: never worth a token, so the tail skips them.
NOISE = re.compile("|".join((
    r"^test .* \.\.\. ok$",                          # cargo test
    r"^test .* \.\.\. ignored",
    r"^\s*(Compiling|Checking|Finished|Fresh|Downloaded|Downloading|Running|Doc-tests|Blocking|Locking|Updating) ",  # cargo
    r"^ok \d+ ",                                       # node:test, TAP
    r"^Test Case .* passed",                           # XCTest
    r"^\s*✔ ",                                         # Swift Testing
    r"^\S.* \.\.\. ok$",                              # unittest -v
    r"^Test (Suite|Case) .* started",                  # XCTest
    r"^Test Suite .* passed",
    # xcodebuild's build steps, and the indented commands it prints under each.
    r"^(CompileSwift|CompileC|SwiftCompile|SwiftDriver|SwiftEmitModule|EmitSwiftModule|SwiftMergeGeneratedHeaders|"
    r"Ld|Libtool|CodeSign|CpResource|CpHeader|Copy|CopySwiftLibs|PhaseScriptExecution|ProcessInfoPlistFile|"
    r"ProcessProductPackaging|ProcessXCFramework|GenerateDSYMFile|Touch|RegisterExecutionPolicyException|Validate|"
    r"WriteAuxiliaryFile|MkDir|CreateBuildDirectory|ExtractAppIntentsMetadata|CompileAssetCatalog|"
    r"LinkAssetCatalog|CompileStoryboard|CompileXCStrings|GenerateAssetSymbols|ScanDependencies|"
    r"ComputePackagePrebuildTargetDependencyGraph|ClangStatCache|PrecompileModule|ExecuteExternalTool) ",
    r"^    (cd |export |builtin-|/Applications/Xcode|/usr/bin/)",
    r"^\s*$",
)))
# Single lines worth showing with the one before them.
LINES = [re.compile(pattern) for pattern in (
    r"^test .* \.\.\. FAILED$",                       # cargo test, one failed test
    r"panicked at ",                                  # a Rust panic
    r"^Traceback \(most recent call last\):",         # Python
    r"^\S.*: error: ",                                # clang, swiftc, xcodebuild: path:line:col: error: …
    r"^error: ",                                      # cargo, git, xcodebuild
    r"^\*\* [A-Z ]*FAILED \*\*",                      # xcodebuild's verdict
    r"^Test Case .* failed",                          # XCTest
    r"^\s*✘ ",                                        # Swift Testing
    r"^Failing tests:",                               # xcodebuild's summary
    r"^\s*✖ ",                                        # node:test's spec reporter (U+2716)
    r"^\S.*\(\d+,\d+\): error TS\d+: ",               # tsc: path(line,col): error TS2345: …
    r"^not ok \d+",                                   # node:test, TAP
    r"^npm ERR! |^npm error ",                        # npm
    r"^FAILED \(",                                    # unittest's verdict
    r"^CI-LOCAL: FAILED",                             # ci-local's own
)]


def excerpt(lines, limit=DEFAULT_MAX, tail=DEFAULT_TAIL):
    """The lines that explain a failure, then the last `tail` lines, at most `limit` in all, in the log's order.

    Returns a list of strings; a gap between two kept ranges is shown as `…`, and lines left out for the limit are
    counted on a last line."""
    keep = set()
    index, count = 0, len(lines)
    while index < count:
        line = lines[index]
        end = None
        if BLOCK_TO_BLANK.match(line):
            end = index
            while end + 1 < count and end - index < BLOCK_MAX and lines[end + 1].strip():
                end += 1
        elif CARGO_FAILURES.match(line):
            end = index
            while end + 1 < count and not CARGO_RESULT.match(lines[end]):
                end += 1
        elif UNITTEST_BLOCK.match(line):
            end = min(index + 1, count - 1)
            while end + 1 < count and not UNITTEST_RULE.match(lines[end + 1]) and not UNITTEST_BLOCK.match(
                    lines[end + 1]):
                end += 1
        elif any(pattern.search(line) for pattern in LINES):
            if index and not NOISE.search(lines[index - 1]):
                keep.add(index - 1)
            end = index
        if end is None:
            index += 1
            continue
        keep.update(range(index, end + 1))
        index = end + 1
    tail_lines = set()
    for number in range(count - 1, -1, -1):
        if len(tail_lines) >= tail:
            break
        if not NOISE.search(lines[number]):
            tail_lines.add(number)
    # The tail is kept whole; the blocks fill what the limit leaves, from the top.
    room = max(limit - len(tail_lines), 0)
    blocks = sorted(keep - tail_lines)
    omitted = max(len(blocks) - room, 0)
    chosen = sorted(set(blocks[:room]) | tail_lines)
    out, previous = [], -1
    for number in chosen:
        if previous >= 0 and number != previous + 1:
            out.append("…")
        out.append(lines[number])
        previous = number
    if chosen and chosen[0] > 0:
        out.insert(0, "…")
    if omitted:
        out.append(f"… {omitted} more matching lines left out (the limit is {limit})")
    return out


def slug(text):
    return re.sub(r"[^A-Za-z0-9._-]+", "-", text).strip("-")[:80] or "command"


def parse(argv):
    """(label, log, limit, tail, command); ValueError on a usage error, None for --help."""
    label = log = None
    limit, tail = DEFAULT_MAX, DEFAULT_TAIL
    args = list(argv)
    while args:
        arg = args.pop(0)
        if arg in ("-h", "--help"):
            return None
        if arg == "--":
            break
        if arg in ("--label", "--log", "--max", "--tail"):
            if not args:
                raise ValueError(f"{arg} takes a value")
            value = args.pop(0)
            if arg == "--label":
                label = value
            elif arg == "--log":
                log = value
            else:
                if not value.isdigit():
                    raise ValueError(f"{arg} takes a whole number, not '{value}'")
                if arg == "--max":
                    limit = int(value)
                else:
                    tail = int(value)
        else:
            raise ValueError(f"unknown argument: {arg} (the command goes after --)")
    if not args:
        raise ValueError("no command: put it after --")
    return label or " ".join(args), log, limit, tail, args


def log_path(label, log, env, checkout_root=CHECKOUT_ROOT):
    if log:
        return Path(log)
    base = env.get("ACS_QUIET_DIR") or str(Path(checkout_root) / "build" / "quiet")
    return Path(base) / f"{slug(label)}.log"


def main(argv, env=os.environ, out=sys.stdout, checkout_root=CHECKOUT_ROOT):
    try:
        parsed = parse(argv)
    except ValueError as error:
        print("usage: scripts/quiet.sh [--label NAME] [--log FILE] [--max N] [--tail N] -- <command>...",
              file=sys.stderr)
        print(f"quiet.sh: {error}", file=sys.stderr)
        return 64
    if parsed is None:
        out.write(__doc__)
        return 0
    label, log, limit, tail, command = parsed
    if env.get("ACS_QUIET") == "0":
        try:
            return subprocess.call(command)
        except OSError as error:
            print(f"quiet.sh: cannot run {command[0]}: {error}", file=sys.stderr)
            return 127
    path = log_path(label, log, env, checkout_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with path.open("w+", encoding="utf-8", errors="replace") as handle:
        try:
            process = subprocess.Popen(command, stdout=handle, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        except OSError as error:
            out.write(f"FAILED (cannot run {command[0]}: {error}): {label}\n")
            return 127
        try:
            status = process.wait()
        except KeyboardInterrupt:
            process.send_signal(signal.SIGINT)
            process.wait()
            out.write(f"interrupted: {label} (log {path})\n")
            return 130
    seconds = time.monotonic() - started
    if status == 0:
        out.write(f"ok: {label} ({seconds:.1f} s; log {path})\n")
        return 0
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    out.write(f"FAILED (exit {status}): {label} ({seconds:.1f} s)\n")
    for line in excerpt(lines, limit, tail):
        out.write(line + "\n")
    out.write(f"full log ({len(lines)} lines): {path}\n")
    out.flush()
    return status if status > 0 else 128 - status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
