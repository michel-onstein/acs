# acs

**acs** — *Ad-hoc Connectivity Shell*. Persistent remote shells over ssh that
survive network drops — with an **unfiltered** terminal stream, so the local
terminal's scrollback, mouse
reporting, OSC 52 copy, hyperlinks and keyboard protocols work exactly as over
plain `ssh -t`. One binary is both the local client and the remote session
holder; it installs itself on the remote the first time you connect.

It replaces the `dsh` shell function (`ssh` + `dtach`). The design is in
[docs/DESIGN.md](docs/DESIGN.md).

## Install

Download the archive for your machine from
[Releases](https://github.com/michel-onstein/acs/releases) (macOS Apple
silicon and Intel, Linux x86_64 and aarch64):

```sh
v=0.1.0 t=aarch64-apple-darwin      # see the release page for the latest
curl -LO https://github.com/michel-onstein/acs/releases/download/v$v/acs-$v-$t.tar.gz
tar xzf acs-$v-$t.tar.gz && install -m 755 acs-$v-$t/acs ~/.local/bin/acs
```

Or build it yourself:

```sh
cargo xtask dist                                  # every target, see below
cp dist/aarch64-apple-darwin/acs ~/.local/bin/    # or the build for your machine
```

`cargo xtask dist` needs `cargo-zigbuild`, `zig` and the Rust targets
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (and on macOS
`x86_64-apple-darwin`). The **complete** binaries in `dist/` carry static
Linux builds for x86_64 and aarch64, so any of them can install acs on any
Linux host. A plain `cargo build` binary is **slim**: it can only install a
copy of itself (same OS and CPU).

Nothing is needed on the remote beyond ssh, a POSIX `sh` and `gzip`: on first
contact acs installs its own version under `~/.local/share/acs/<version>/`
and links `~/.local/bin/acs` to it.

## Use

```text
acs [ssh options] [user@]host [session]   attach, or create (default session: main)
acs [ssh options] [user@]host --new       create a new numbered session (1, 2, …)
acs [ssh options] [user@]host --list      list sessions on host
acs host session -- command args…         run a command instead of the login shell
```

In a session, press **Ctrl-] Ctrl-]** quickly, then:

| Key | Effect |
| --- | --- |
| `d` | detach — the session keeps running; reattach with `acs host session` |
| `x` | exit — end the session on the remote |

A single Ctrl-], or Ctrl-] followed by any other key, goes to the program as
usual. The command key works in every keyboard encoding a terminal may use
(including the kitty keyboard protocol) and never triggers inside a paste.

### ssh options

`-i <identity_file>`, `-p <port>`, `-J <jump>`, `-F <config>` and
`-o <option=value>` are passed to every ssh call acs makes, with ssh's
meaning. `-l` is `--list` (as in `dsh`); put a login name in `user@host` or
`-o User=`. Your `~/.ssh/config`, agent and keys apply as usual; nothing
needs configuring on either side.

### When the network drops

The client notices a dead link within 10 seconds, redials with backoff
(1 s doubling to 30 s, or at once when your network changes) and resumes
exactly where the output stopped — nothing is lost or repeated. While it is
disconnected a status line shows at the bottom of the screen, typed keys are
dropped, and Ctrl-] Ctrl-] `d` still detaches. `--no-reconnect` exits
instead.

### Several people, one account

Each client has an identity (`user@hostname`, or `ACS_IDENTITY`). Attaching
to a session someone else is attached to asks first:

```text
acs: session 'main' on devbox is attached from alice@laptop since 10:02 — take over? [y/N]
```

`--force` skips the question. `ACS_DEFAULT_SESSION` gives each person their
own default instead of `main`.

## Environment

| Variable | Meaning |
| --- | --- |
| `ACS_DEFAULT_SESSION` | session plain `acs host` means (default `main`) |
| `ACS_IDENTITY` | identity shown to others on a shared account |
| `ACS_ESCAPE_KEY` | command key in `^X` notation (default `^]`) |
| `ACS_ESCAPE_TIMEOUT_MS` | window for the double press (default 400) |
| `ACS_SSH` | ssh program (default `ssh`; also `--ssh`) |
| `ACS_SOCKET_DIR` | remote socket directory (default `/tmp/acs-<uid>`) |
| `ACS_RING` | remote output history kept for resume, bytes (default 1 MiB) |

## Development

```sh
scripts/verify.sh       # fmt, clippy (macOS and Linux targets), tests, markdownlint
scripts/test_linux.sh   # the suite on Linux in a container, plus multi-user isolation
scripts/e2e_ssh.sh      # end to end over real ssh against a container host
scripts/version-bump.sh # release the next version and publish its binaries
scripts/release-binaries.sh  # (re)publish a tag's binaries to GitHub Releases
```

Results of the checks that need a real terminal are in
[docs/VERIFICATION.md](docs/VERIFICATION.md).
