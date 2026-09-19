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
  as `acs -F /dev/null -i <key> -p <port> … dev@127.0.0.1` (a free port docker
  picks, or `ACS_E2E_PORT`).
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
| `acs list <host>` | over ssh after the sessions above | Pass — sessions listed |
| `acs list` on every alias | no host; aliases `box` (the container) and `gone` (192.0.2.1, answers no ping); ssh in `BatchMode` | Pass — `box` sessions under a HOST column, one stderr line for `gone`, exit 255 |
| `user@<alias>` | `acs dev@box` where alias `box`'s only entry says `user: nobody` | Pass — logs in as `dev` (2026-09-18) |
| Ctrl-L after reconnecting | a program reporting each byte it receives: a new session, then a re-attach, then the `sshd-session` killed | Pass — nothing to the new session, one `0c` after the re-attach and one after the resume (2026-09-18) |
| `identity_file` | no `-i`; the key only at `~/.ssh/id_box` under a HOME of the test's own, named by a host entry over a missing alias key, then by an alias | Pass — both log in; acs expands the `~` (2026-09-18) |
| Session menu | plain `acs dev@127.0.0.1` with sessions `menu-a` and `menu-b` detached: cursor to `menu-b`, `x`, `y`, then `menu-a`'s number | Pass — `menu-b` ended and gone from the menu, `menu-a` attached (2026-09-18); with `-v`, one `running ssh … _proxy --pick` from the list to the attach (2026-09-19) |
| Full test suite on Linux | `scripts/test_linux.sh` | Pass — found and fixed two Linux-only bugs first (a master stall under backpressure; a replaced binary breaking master start) |
| Multi-user isolation | squatted and symlinked socket directories, foreign peer uid, per-user `main` | Pass |
| Static Linux binaries run | `dist/*-linux-musl/acs --version` in Alpine (aarch64 native, x86_64 emulated) | Pass |
| macOS binaries signed | `codesign -v` on both complete macOS builds | Pass |
| One-line installer | `scripts/test_install.sh` against the real v0.1.0 and v0.2.0 releases: macOS arm64 (curl), Alpine x86_64 (busybox wget) and Ubuntu aarch64 (curl), each as root and as a user | Pass — default layout, pinned version and in-place upgrade, `ACS_INSTALL_DIR`, checksum mismatch, missing version, unsupported platform |
| `acs upgrade` from GitHub, macOS | this code built as 0.1.0 (plain file), `acs upgrade --check`, then `acs upgrade` (2026-09-18) | Pass — replaced by the real 0.2.0 release; `codesign -v`: valid on disk, satisfies its Designated Requirement; no quarantine attribute |
| `acs upgrade` from GitHub, Linux | the same as a static aarch64 build in Alpine, which has no curl | Pass — downloaded with the wget fallback and replaced by 0.2.0 |
| Homebrew, macOS | `brew install michel-onstein/acs/acs` (the v0.3.0 formula from the tap), `brew test acs`, `brew audit --strict --online`, `brew style`, on macOS arm64 (2026-09-18) | Pass — installed from the release archive into the Cellar, `codesign -v` valid, `acs --version` 0.3.0 with both Linux remotes; audit and style clean |
| Homebrew, Linux | the same in the `homebrew/brew` container, aarch64 and x86_64 (emulated) | Pass — the static build installs and its test passes; the appended payloads survive brew's install |

## Still to check by hand

These need a person at a real terminal or a real network, and are not
claimed as verified:

- **Rendering and local scrollback** in the terminal you use (Ghostty,
  iTerm2, kitty, …). The byte stream is proven identical to the program's
  output, so this should hold; it has not been looked at.
- **Claude Code** as the remote program (mouse selection, auto copy). Not
  installable in the throwaway host.
- **The command-mode bell** in a real terminal (heard, or flashed, as the
  terminal is set up). Tests show the BEL byte arrives, and never inside an
  OSC the program is sending.
- **The Ctrl-L repaint** after a re-attach and a resume, in zsh, bash, vim,
  less and htop at a real terminal. Tests show the byte arrives once per
  reconnect and in order; how each program repaints has not been looked at.
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
