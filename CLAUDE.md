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
  multi-user isolation as root.
- `scripts/e2e_ssh.sh` — end to end over real ssh against a container host.

## Versioning

The version moves automatically after every merge to `main`
(`scripts/version-bump.sh`, run by the ship workflow): PATCH for a small fix,
MINOR for a feature or a larger fix, MAJOR only with `--major`. **Title pull
requests in Conventional Commits form** (`fix: …`, `feat: …`, `docs: …`) —
the squash commit takes the title — and add a `Semver: minor` trailer or a
`semver:minor` label to a fix that deserves a minor release. Rules in
[docs/VERSIONING.md](docs/VERSIONING.md).

## Document index

| Doc | Description | Status |
| --- | --- | --- |
| [README.md](README.md) | Install, usage, command keys, environment, migration from dsh | Current |
| [DESIGN.md](docs/DESIGN.md) | Architecture, protocol, command mode, remote install | Built |
| [VERSIONING.md](docs/VERSIONING.md) | Automatic semantic versioning: rules and the bump script | Built |
| [VERIFICATION.md](docs/VERIFICATION.md) | End-to-end results over real ssh; checks still to do by hand | Automated checks pass |
