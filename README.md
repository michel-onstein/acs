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

```sh
curl -fsSL https://github.com/michel-onstein/acs/releases/latest/download/install.sh | sh
```

The installer picks the build for your machine (macOS Apple silicon and
Intel, Linux x86_64 and aarch64), checks it against the release's
`SHA256SUMS`, and installs it the way acs installs itself on a remote:

| Run as | Binary | On `PATH` as |
| --- | --- | --- |
| you | `~/.local/share/acs/<version>/acs` | `~/.local/bin/acs` (a link) |
| root | `/usr/local/lib/acs/<version>/acs` | `/usr/local/bin/acs` (a link) |

so a client of the same version finds acs already there when it connects
to this machine. It says how to add the directory to your `PATH` if it is
not on it, and running it again upgrades in place. It needs `curl` or
`wget`, and `sha256sum`, `shasum` or `openssl`.

| Variable | Meaning |
| --- | --- |
| `ACS_VERSION=0.2.0` | install that release instead of the latest |
| `ACS_INSTALL_DIR=~/bin` | put the binary itself in that directory |

for example `curl -fsSL …/install.sh | ACS_VERSION=0.2.0 sh`. By hand, the
archives are on [Releases](https://github.com/michel-onstein/acs/releases):

```sh
v=0.2.0 t=aarch64-apple-darwin      # see the release page for the latest
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

### Upgrade

```sh
acs upgrade            # the latest release, checked against its SHA256SUMS
acs upgrade --check    # only say whether there is a newer one
acs upgrade --version 0.2.0   # that release, even an older one
```

acs replaces itself in place (a link like `~/.local/bin/acs` is pointed at
the new version); if its directory is not yours, it says to use
`sudo acs upgrade`. It uses `curl` (or `wget`). Remote hosts need nothing:
the next connection installs the new version there.

Once a week acs looks for a newer release in the background (it never
delays connecting) and, if there is one, says so once when you next start
it:

```text
acs: acs 0.3.0 is available (you have 0.2.0) — run: acs upgrade
```

Offline, it says nothing. Turn it off with `ACS_NO_UPDATE_CHECK=1` or
`update_check: false` in the configuration.

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

The terminal bell rings when Ctrl-] Ctrl-] has armed command mode, so you
know the next key is a command (command mode waits 2 seconds for it). acs
writes the bell to your terminal only, never to the program, and never in
the middle of a sequence the program is sending. Turn it off with
`command_bell: false` in the configuration or `ACS_COMMAND_BELL=0`.

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

## Configuration

Settings live in YAML files: the global `/etc/acs/config.yaml`, then your
own `~/.config/acs/config.yaml` (or `$XDG_CONFIG_HOME/acs/config.yaml`).
Both are optional. A setting in your file replaces the global one; mappings
merge key by key and lists add up, global entries first.

```yaml
# ~/.config/acs/config.yaml
install_on_remote: false   # never install acs on a host (default: true)
update_check: false        # never look for a newer release (default: true)
command_bell: false        # no bell when Ctrl-] Ctrl-] arms (default: true)
hosts:
  devbox:                  # acs devbox
    - host: devbox.lan     # at home: used if it answers a ping
    - host: devbox.example.com
      user: michel         # from outside, as michel
  nas:
    - host: nas.lan
      reachability_check: false   # it drops pings: use it unchecked
```

| Setting | Meaning |
| --- | --- |
| `install_on_remote` | install acs on a host that lacks it (default `true`); when `false`, acs says what is missing and exits with 254 |
| `update_check` | look for a newer acs release once a week (default `true`) |
| `command_bell` | ring the terminal bell when Ctrl-] Ctrl-] arms command mode (default `true`; `ACS_COMMAND_BELL` overrides it) |
| `hosts` | aliases: each name maps to a list of `host` entries, with an optional `user` and `reachability_check` (default `true`) |

A mistake in a file stops acs with the file and line, for example
`~/.config/acs/config.yaml:2: install_on_remote: expected true or false`.

### Host aliases

`acs devbox` with the file above pings `devbox.lan` once; if it answers, acs
connects there, otherwise it tries `michel@devbox.example.com`. An entry
with `reachability_check: false` is used without a ping. Without a `user`,
your `~/.ssh/config` picks the login name. If no entry answers, acs lists the
hosts it tried and exits with 255. `-v` shows which entry was chosen and why.

The alias is resolved again on every reconnect, so when you move from home
to outside, the redial goes to whichever address answers. List ways of
reaching **one** machine under an alias: the session lives on that machine,
so a fallback to a different one finds no session to resume.
`me@devbox`, or any name that is not an alias, is used as given.

### Editing it from the command line

```sh
acs config host add devbox devbox.lan                 # the first host of devbox
acs config host add devbox devbox.example.com --user michel
acs config host add nas nas.lan --no-reachability-check
acs config host list                                  # aliases and their hosts
acs config host remove devbox devbox.lan              # one host, or the alias
acs config set install_on_remote false
acs config set update_check false                     # no weekly release check
acs config set command_bell false                     # no bell for command mode
acs config get install_on_remote
acs config unset install_on_remote                    # back to the default
acs config show                                       # everything, and where it is from
acs config path                                       # the files acs reads
```

These edit `~/.config/acs/config.yaml`; add `--global` for
`/etc/acs/config.yaml` (with sudo). Comments and the order of the rest of the
file are kept. Because `config` is a command, a host named `config` is
reached as `user@config`.

## Environment

| Variable | Meaning |
| --- | --- |
| `ACS_DEFAULT_SESSION` | session plain `acs host` means (default `main`) |
| `ACS_IDENTITY` | identity shown to others on a shared account |
| `ACS_ESCAPE_KEY` | command key in `^X` notation (default `^]`) |
| `ACS_ESCAPE_TIMEOUT_MS` | window for the double press (default 400) |
| `ACS_COMMAND_BELL` | `0`: no bell when command mode arms; `1`: a bell even if the configuration turns it off |
| `ACS_SSH` | ssh program (default `ssh`; also `--ssh`) |
| `ACS_SOCKET_DIR` | remote socket directory (default `/tmp/acs-<uid>`) |
| `ACS_RING` | remote output history kept for resume, bytes (default 1 MiB) |
| `ACS_DIAL_TIMEOUT_MS` | how long a connection may take to answer (default 120 s at first, 30 s on a redial) |
| `XDG_CONFIG_HOME` | where your configuration file is (default `~/.config`) |
| `ACS_GLOBAL_CONFIG` | global configuration file (default `/etc/acs/config.yaml`) |
| `ACS_PING` | ping program for alias reachability checks (default `ping`) |
| `ACS_RELEASES_URL` | where `acs upgrade` and the update check look for releases (default GitHub) |
| `ACS_NO_UPDATE_CHECK` | `1`: never look for a newer release |
| `XDG_STATE_HOME` | where the update check keeps its state (default `~/.local/state`) |

## Development

```sh
scripts/verify.sh       # fmt, clippy (macOS and Linux targets), tests, markdownlint
scripts/test_linux.sh   # the suite on Linux in a container, plus multi-user isolation
scripts/e2e_ssh.sh      # end to end over real ssh against a container host
scripts/test_install.sh # install.sh against the real releases, here and in containers
scripts/version-bump.sh # release the next version and publish its binaries
scripts/release-binaries.sh  # (re)publish a tag's binaries to GitHub Releases
```

Results of the checks that need a real terminal are in
[docs/VERIFICATION.md](docs/VERIFICATION.md).
