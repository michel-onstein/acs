# acs — verification

**Status:** Automated end-to-end checks pass (2026-09-18); the checks that
need a person at a real terminal are listed at the end, not yet done.

What the unit and integration tests cannot show is checked here: acs over a
real ssh connection, against a real host, with real terminal programs and
real interruptions. `scripts/e2e_ssh.sh` automates everything that can be
automated; `scripts/test_linux.sh` covers Linux and multi-user isolation.

## Environment

- Client: `dist/aarch64-apple-darwin/acs` (complete build from
  `cargo xtask dist`), macOS on Apple silicon.
- Host: Alpine Linux container (`scripts/e2e/Dockerfile`) running OpenSSH
  `sshd`, user `dev` accepting only a freshly generated ed25519 key, reached
  as `acs -F /dev/null -i <key> -p 2222 … dev@127.0.0.1`.
- Linux suite: `rust:alpine` container, musl, run as root with users `alice`
  and `bob`.

## Results

| Check | How | Result |
| --- | --- | --- |
| First contact installs acs | fresh host; client streams its Linux aarch64 payload, `_install --finish` completes it | Pass — installed, session attached |
| `-i` selects the key | same command without `-i` (`IdentitiesOnly=yes`, `BatchMode=yes`) | Pass — exit 255 without it, works with it |
| vim | `vim -u NONE -N`, then `:q` | Pass — alternate screen entered and left, exit status 0, terminal restored |
| OSC 52 and OSC 8 | program prints a clipboard write and a hyperlink | Pass — both byte-exact at the client |
| Kitty keyboard protocol | program pushes kitty flags; client sends Ctrl-] Ctrl-] d as `CSI 93;5u` | Pass — detaches; the pushed flags are popped on the way out |
| Mouse reports | program enables mode 1000; client sends an X10 report | Pass — program receives `1b 5b 4d 20 21 21` |
| Killed connection | the host's `sshd-session` for the connection is killed mid-stream | Pass — reconnect, numbered output consecutive, nothing lost or repeated |
| Frozen host | `docker pause` for 4 s beyond the dead-link timeout | Pass — dead link detected in 3.0 s (`ACS_DEAD_MS=3000`), resumed with no loss |
| `--list` | over ssh after the sessions above | Pass — sessions listed |
| Full test suite on Linux | `scripts/test_linux.sh` | Pass — found and fixed two Linux-only bugs first (a master stall under backpressure; a replaced binary breaking master start) |
| Multi-user isolation | squatted and symlinked socket directories, foreign peer uid, per-user `main` | Pass |
| Static Linux binaries run | `dist/*-linux-musl/acs --version` in Alpine (aarch64 native, x86_64 emulated) | Pass |
| macOS binaries signed | `codesign -v` on both complete macOS builds | Pass |

## Still to check by hand

These need a person at a real terminal or a real network, and are not
claimed as verified:

- **Rendering and local scrollback** in the terminal you use (Ghostty,
  iTerm2, kitty, …). The byte stream is proven identical to the program's
  output, so this should hold; it has not been looked at.
- **Claude Code** as the remote program (mouse selection, auto copy). Not
  installable in the throwaway host.
- **htop and less** interactively. Covered only indirectly (vim, raw mouse and
  key paths).
- **A real Wi-Fi switch and laptop sleep/wake.** Simulated by killing and
  freezing the connection and by the network-change hook (`ACS_NETWATCH_FIFO`);
  the real events have not been exercised.
- **A real host that needs `-i`, such as corello.** The container host proves
  `-i` passthrough; connecting to a work host installs acs there, which is
  the owner's call.

To run the automated part again: `scripts/e2e_ssh.sh` (add `--no-build` to
reuse `dist/`).
