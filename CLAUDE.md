# acs

**acs** — *Ad-hoc Connectivity Shell*. Persistent, reconnecting remote shells
over ssh with an unfiltered terminal stream — one Rust binary for both the local
client and the remote session holder. Replaces the `dsh` shell function (`ssh` +
`dtach`).

Work is tracked as beads (`br`, prefix `acs`): start with `bv --robot-triage`.

## Build and verify

- `scripts/verify.sh` — fmt, clippy (`-D warnings`), tests, markdownlint;
  must pass before landing.
- `cargo zigbuild --release --target x86_64-unknown-linux-musl` — static
  Linux build (needs `zig`).
- `cargo xtask dist` — every target plus the embedded payloads (DESIGN §8.1).
- `scripts/test_linux.sh` — the suite on Linux in a container, plus
  multi-user isolation as root and the network watcher against the real
  kernel (`tests/netns.rs`, needs `CAP_NET_ADMIN`); `--only NAME` runs one.
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

## Document index

| Doc | Description | Status |
| --- | --- | --- |
| [README.md](README.md) | Install, usage, command keys, environment | Current |
| [DESIGN.md](docs/DESIGN.md) | Architecture, protocol, command mode, remote install | Built |
| [VERSIONING.md](docs/VERSIONING.md) | Automatic semantic versioning, release binaries on GitHub, the Homebrew tap, what a fork must point at itself | Built |
| [VERIFICATION.md](docs/VERIFICATION.md) | End-to-end results over real ssh; checks still to do by hand | Automated checks pass |
| [ACSCP_DESIGN.md](docs/ACSCP_DESIGN.md) | `acscp`: scp for acs — a fourth role of the one binary under a second name, resumable copies, the remote end self-installed | Proposed |
