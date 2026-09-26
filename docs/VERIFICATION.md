# acs — verification

**Status:** Automated end-to-end checks pass (2026-09-24), except
`scripts/test_install.sh`, which has been stale since release signing landed
and cannot pass as it stands (see its rows below); the checks that need a
person at a real terminal are listed at the end, not yet done.

What the unit and integration tests cannot show is checked here: acs over a
real ssh connection, against a real host, with real terminal programs and
real interruptions. `scripts/e2e_ssh.sh` automates everything that can be
automated; `scripts/test_linux.sh` covers Linux, multi-user isolation, and
the network watcher against the real kernel.

## Environment

- Client: `dist/aarch64-apple-darwin/acs` (complete build from
  `cargo xtask dist`), macOS on Apple silicon.
- Host: Alpine Linux container (`scripts/e2e/Dockerfile`) running OpenSSH
  `sshd`, user `dev` accepting only a freshly generated ed25519 key, reached
  as `acs -F /dev/null -i <key> -p <port> … dev@127.0.0.1` (a free port docker
  picks, or `ACS_E2E_PORT`). The image sets `AllowTcpForwarding yes`, which
  Alpine ships as `no`, so that `-L` can be exercised at all; that is a test
  fixture and not something acs installs or asks a host for.
- Linux suite: `rust:alpine` container, musl, run as root with users `alice`
  and `bob`.

## Results

| Check | How | Result |
| --- | --- | --- |
| First contact installs acs | fresh host; client streams its Linux aarch64 payload, `_install --finish` completes it | Pass — installed, session attached |
| `-i` selects the key | same command without `-i` (`IdentitiesOnly=yes`, `BatchMode=yes`) | Pass — exit 255 without it, works with it |
| `-L` forwards a port, and only on the session's ssh | `scripts/e2e_ssh.sh` (`e2e_14`): `acs -L <free port>:127.0.0.1:22 … dev@127.0.0.1 fwd`, the port read with a plain TCP connect, `acs list` given the same `-L` while the session holds the port, then the session's ssh killed to force a redial (2026-09-23) | Pass — the container's sshd banner comes back through the port; the `acs list` side call succeeds and its stderr never names the port, so its ssh never asked to bind it; after the redial a new ssh rebinds the port and the banner comes back |
| The ssh master acs owns | `scripts/e2e_ssh.sh` (`e2e_15`, acs-9n3): a session, detached, then the same `acs` again; `ssh -O check` on acs's control socket between each step; then the host's `sshd-session` killed to force a redial (2026-09-23) | Pass — the detach leaves one master (`0600` socket in a `0700` directory), the reattach is served by that same pid rather than a second one, and the redial after the kill ends it (`-O check` then fails). Measured: 51 ms to `ACS-READY` cold against 13 ms on the master, over loopback where a handshake costs almost no round trips |
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
| The prelude's check of the binary it execs | `ssh::tests` on macOS (BSD `ls`) and in `rust:alpine` (busybox `ls`), plus the shell walked by hand against GNU coreutils 9.1 in Debian (2026-09-24, acs-gov): a real binary and a symlinked one, each with a safe and an unsafe target, and a relative link, a chain of links, a link into a world-writable directory, a link *in* one, and a dangling link | Pass on both platforms — the mode and owner examined are the target's, and every directory the link passes through is checked. Shown red first by putting the old check back: it refused the safe symlink on Linux (with a message about permissions that were not the problem) and ran the world-writable one on macOS without a word. `ls -ldnL` follows the link identically on busybox 1.36.1, coreutils 9.1 and BSD `ls`; the three differ only on a dangling link, which `[ -x ]` rejects before the mode is ever read |
| The system-wide install path, really populated by root | `tests/multiuser.rs` under `scripts/test_linux.sh` (2026-09-24, acs-6w9): root writes `/usr/local/lib/acs/<version>/acs` and alice and bob run the prelude against it; then the directory made `0777` and `0775`, the binary made `0775`, the file chowned to bob, and the directory chowned to bob; and the same owners tried at the `$HOME` candidate | Pass — both users exec root's binary, and every loosening is refused by name: the writable cases say "is writable by others" with the mode, the third-uid cases say "is owned by uid N, not by you or root", and each prints `ACS-NEED` so the client installs instead. Shown red first by putting acs-08m's `= $acs_u` back alone: root's binary was refused to alice at both paths, which is the defect |
| The network watcher's platform half, Linux netlink | `tests/netns.rs` under `scripts/test_linux.sh` (2026-09-24, acs-4i2): a dummy interface created, brought up, addressed with `192.0.2.7/24` and torn down again in the container's own network namespace (`--cap-add NET_ADMIN`), against a real `NETLINK_ROUTE` watcher | Pass — the descriptor becomes readable after each `ip` command, and the test never writes to it, so the message is the kernel's own multicast; a bare interface is a message and not a change, an addressed one is both, and the address going away is a change again. Since acs-0n8 it also puts the address back and drops the answer without acting on it, and the interface going away again is then *not* a change — the networks held move where a caller acts and nowhere else. Shown red both ways first: subscribed to no netlink group, the kernel "said nothing about an interface appearing"; with acs-6p8's comparison removed, "an interface with no address is not a change" failed |
| Full test suite on Linux | `scripts/test_linux.sh` (2026-09-24, acs-cu8) | Pass, 36 tests back from red — found and fixed two Linux-only bugs first (a master stall under backpressure; a replaced binary breaking master start), and later 36 failures that were the *harness* being platform dependent: `ssh-keygen` is not in `rust:alpine` (25), and the fake remote's acs was a symlink, whose mode is `lrwxrwxrwx` on Linux, so the prelude's safety check refused it and every one of those remotes looked uninstalled (11) |
| Multi-user isolation | squatted and symlinked socket directories, foreign peer uid, per-user `main`, and (acs-9n3) a control-socket directory alice created before bob | Pass — bob refuses alice's directory by name and dials with no control path at all; his own comes up `0700` and his, and alice cannot list it (`rust:alpine` as root, `cargo test --test multiuser`, 2026-09-23) |
| Static Linux binaries run | `dist/*-linux-musl/acs --version` in Alpine (aarch64 native, x86_64 emulated) | Pass |
| macOS binaries signed | `codesign -v` on both complete macOS builds | Pass |
| One-line installer | `scripts/test_install.sh` against the real v0.1.0 and v0.2.0 releases: macOS arm64 (curl), Alpine x86_64 (busybox wget) and Ubuntu aarch64 (curl), each as root and as a user | Pass — default layout, pinned version and in-place upgrade, `ACS_INSTALL_DIR`, checksum mismatch, missing version, unsupported platform. **Stale since signing (acs-o9v): the suite cannot pass today.** Its default `ACS_TEST_OLD=0.1.0` predates `SHA256SUMS.sig`, so the pinned-version case dies on a 404, and the checksum-mismatch mirror it builds has no `.sig` either, so the installer refuses before the checksum is reached. Both reproduce unchanged on `origin/main` |
| One-line installer, after acs-x57 | the substituted script by hand (2026-09-25): `shellcheck` and `sh -n`; `scripts/test_install.sh` as far as the stale cases allow (latest into the default layout, pinned v0.15.0 then upgraded in place, `ACS_INSTALL_DIR`); the repository script and the **packaged** one run for real in `alpine:3` (busybox wget, ash) and `ubuntu:24.04`, as root and as a user; a fork-packaged copy run locally | Pass — all three reachable cases green, every container install landed the 0.16.0 release and ran it, and the fork's copy asks `https://github.com/someone/acs-fork/...` and nothing upstream |
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
- **A real Wi-Fi switch and laptop sleep/wake, on macOS.** Narrowed by
  acs-4i2, and no longer the same item on both platforms:
  - **Linux netlink is no longer by hand.** `tests/netns.rs` (the row
    above) makes the kernel create, address and remove a real interface and
    checks that the watcher's socket hears it and that `local_networks()`
    moves with it.
  - **macOS `PF_ROUTE` stays by hand**, and there is no prospect of
    automating it: the test would need the machine running it to really
    join another network, which CI cannot do. What is unproven is that a
    real roam, wake or VPN on macOS emits a message
    `netwatch::route_messages_matter` accepts **and** moves an address
    `LocalNet::usable` accepts — not merely that the client reacts when one
    does (that is covered by the stand-ins, `ACS_NETWATCH_FIFO` for the
    kernel's hint and `ACS_NETWATCH_NETS` for the machine's networks).
    Since acs-ft1 the same evidence also shortens the dead-link timeout on
    a link that is still up, so the hand check has a second question: a
    real switch to a network the session survives (a VPN coming up, a
    second interface appearing) must leave the session alone, and a real
    switch it does not survive must be back inside a couple of seconds
    instead of ten.
  - **How to run it, and what a dead watcher now looks like.** Attach a
    session with `-v` and change network. Every hint leaves a line —
    `acs: network changed: <old> → <new>` for one that counted, `acs:
    network hint: still on <nets> — not a change` for one that did not, and
    `acs: network: N bytes from the kernel, no address or interface
    message` where the `PF_ROUTE` filter rejected everything read. A change
    read at a moment the client could not use it adds `acs: network change
    not acted on — the next hint reports it again` (acs-0n8), which is the
    mid-handshake and rate-limited cases and not a failure. *No line at
    all* across a roam is the failure this check exists for: before
    acs-4i2 it was indistinguishable from a network that did not move.
- **A real host that needs `-i`, such as corello.** The container host proves
  `-i` passthrough; connecting to a work host installs acs there, which is
  the owner's call.

To run the automated part again: `scripts/e2e_ssh.sh` (add `--no-build` to
reuse `dist/`, which takes the suite from about a minute to about 20 s).
`--no-build` reuses `dist/` only if `dist/source.stamp` — written by
`cargo xtask dist`, a hash of the sources it built from — matches this tree,
so an edit made since the build is refused rather than silently tested
against the old binary (acs-gb4). A checkout with no `dist/` at all — a fresh
worktree — is built once instead: absent is not stale (acs-0pr). A `dist/`
holding no binary for this host is named and refused. `--allow-stale-dist`
says you mean it, and needs `--no-build` to mean anything.
