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

## Document index

| Doc | Description | Status |
| --- | --- | --- |
| [README.md](README.md) | Install, usage, command keys, environment, migration from dsh | Current |
| [DESIGN.md](docs/DESIGN.md) | Architecture, protocol, command mode, remote install | Built |
| [VERIFICATION.md](docs/VERIFICATION.md) | End-to-end results over real ssh; checks still to do by hand | Automated checks pass |
