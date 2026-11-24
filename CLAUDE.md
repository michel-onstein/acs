# acs

**acs** — *Ad-hoc Connectivity Shell*. Persistent, reconnecting remote shells
over ssh with an unfiltered terminal stream — one Rust binary for both the local
client and the remote session holder. Replaces the `dsh` shell function (`ssh` +
`dtach`).

Work is tracked as beads (`br`, prefix `acs`): start with `bv --robot-triage`.

## Build and verify

- `scripts/verify.sh` — fmt, clippy (`-D warnings`), tests, shellcheck, the
  scripts' Python suites, the project logs, markdownlint; must pass before
  landing. Each step runs through `scripts/quiet.sh`: one `ok:` line when it
  passes, the lines that explain a failure when it fails, the whole output in
  `build/quiet/<step>.log`; `ACS_QUIET=0` streams everything. Run any other
  long command the same way: `scripts/quiet.sh --label NAME -- <command>`.
- `scripts/markdownlint.sh` — markdownlint-cli2 at the version pinned in
  `scripts/lib/markdownlint-cli2.version`; `--fix` fixes what it can.
- `scripts/install-hooks.sh`, once per clone — a commit on `main` in the shared
  checkout then fails; work in a worktree under `.claude/worktrees/`. The
  release already commits from a throwaway worktree, so it is unaffected.
- `cargo zigbuild --release --target x86_64-unknown-linux-musl` — static
  Linux build (needs `zig`).
- `cargo xtask dist` — every target plus the embedded payloads (DESIGN §8.1).
- `scripts/test_linux.sh` — the suite on Linux in a container, plus
  multi-user isolation as root, the network watcher against the real
  kernel (`tests/netns.rs`) and port knocking against a real knockd
  (`tests/knockd.rs`), both needing `CAP_NET_ADMIN`; `--only NAME` runs one.
- `scripts/e2e_ssh.sh` — end to end over real ssh against a container host.
- `scripts/test_install.sh` — the one-line installer against the real
  releases, locally and in Alpine and Ubuntu containers.

## Versioning

**There is no CI** — no `.github/`, so "no checks reported" on a pull request
is expected. `scripts/verify.sh` is the gate, run by whoever lands the change
on their own machine. The version bump is local too: `scripts/version-bump.sh`,
run by the `ship` skill's version-bump step after each merge; skip that and
`main` sits ahead of its last tag until someone bumps (VERSIONING "Who runs
it"). It picks PATCH for a small fix, MINOR for a feature or a larger fix,
MAJOR only with `--major`. **Title pull
requests in Conventional Commits form** (`fix: …`, `feat: …`, `docs: …`) —
the squash commit takes the title — and add a `Semver: minor` trailer or a
`semver:minor` label to a fix that deserves a minor release. Each release's
binaries are published to GitHub Releases by the same script, which then
updates the Homebrew tap (`michel-onstein/homebrew-acs`). Rules in
[docs/VERSIONING.md](docs/VERSIONING.md).

## Project notes

`docs/project_notes/` holds the [bug log](docs/project_notes/bugs/README.md)
and the [work log](docs/project_notes/work_log/README.md), each a directory of
one file per entry: **add a file; never edit a shared one**. Search the bug log
before debugging, add to it after a fix, and add a work-log file with every
change. `scripts/check_project_logs.py`, in the gate, holds the naming.

The shared tooling under `scripts/` that also lives in other Q-Jam
repositories — `quiet.sh`, `markdownlint.sh`, `install-hooks.sh`, the
project-logs check — comes from qjam/project-template; `/sync-conventions`
keeps it in step. Fix it there first.

## Document index

| Doc | Description | Status |
| --- | --- | --- |
| [README.md](README.md) | Install, usage, command keys, environment | Current |
| [DESIGN.md](docs/DESIGN.md) | Architecture, protocol, command mode, remote install | Built |
| [ACSCP_DESIGN.md](docs/ACSCP_DESIGN.md) | acscp / `acs cp`: scp-style copies over acs's dial, resume, relay, wildcards (epic acs-hofu) | Built; by-hand checks pass |
| [KNOCK_DESIGN.md](docs/KNOCK_DESIGN.md) | Port knocking (knockd) before every dial that opens a connection: sequence, timing, proxies, configuration (epic acs-8l1v) | Built; checked against a real knockd on Linux |
| [VERSIONING.md](docs/VERSIONING.md) | Automatic semantic versioning, release tags (bare from 0.19.0), release binaries on GitHub, the Homebrew tap, what a fork must point at itself | Built |
| [bugs/](docs/project_notes/bugs/) | Bug log: one file per bug, each with its regression test | Current |
| [work_log/](docs/project_notes/work_log/) | What landed when: one file per landed change | Current |
| [VERIFICATION.md](docs/VERIFICATION.md) | End-to-end results over real ssh; checks still to do by hand | Automated checks pass |
