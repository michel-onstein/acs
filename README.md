# acs

**acs** — *Ad-hoc Connectivity Shell*. Persistent remote shells over ssh that
survive network drops — with an **unfiltered** terminal stream, so the local
terminal's scrollback, mouse
reporting, OSC 52 copy, hyperlinks and keyboard protocols work exactly as over
plain `ssh -t`. One binary is both the local client and the remote session
holder; it installs itself on the remote the first time you connect.

The design is in [docs/DESIGN.md](docs/DESIGN.md).

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
`wget`, `sha256sum`, `shasum` or `openssl`, and `ssh-keygen` (which comes
with the ssh acs needs anyway).

| Variable | Meaning |
| --- | --- |
| `ACS_VERSION=0.2.0` | install that release instead of the latest |
| `ACS_INSTALL_DIR=~/bin` | put the binary itself in that directory |

for example `curl -fsSL …/install.sh | ACS_VERSION=0.2.0 sh`.

With [Homebrew](https://brew.sh) (macOS, or Homebrew on Linux):

```sh
brew install michel-onstein/acs/acs
```

installs the same build from the tap
[michel-onstein/homebrew-acs](https://github.com/michel-onstein/homebrew-acs),
which every release updates; upgrade it with `brew upgrade acs`.

By hand, the archives are on
[Releases](https://github.com/michel-onstein/acs/releases):

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
acs upgrade            # the latest release, checked against its signed SHA256SUMS
acs upgrade --check    # only say whether there is a newer one
acs upgrade --version 0.2.0   # that release, even an older one
```

### Release signatures

A release's `SHA256SUMS` is signed, and both the installer and `acs
upgrade` check the signature against a key they carry **before** they read
a checksum out of it — a checksum served from the same place as the
archive proves only that the two agree with each other. Signing is
`ssh-keygen -Y` in the `acs-release` namespace, so no extra tool is
needed. A missing signature, one that does not verify, or a missing
`ssh-keygen` stops the install or upgrade; nothing is unpacked or run.

The public key is below and in the binary, and it is published in the
[Homebrew tap](https://github.com/michel-onstein/homebrew-acs) — a
separate repository — so you can check one against the other rather than
trusting only the copy that came with the download:

```text
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILbxQW5C9X7CdwcQ4bab0gsQi4Evk2xfgmI/972dlHCb acs release signing
```

To check a release by hand:

```sh
printf 'releases@acs namespaces="acs-release" %s\n' "$(cat acs-release.pub)" > allowed
ssh-keygen -Y verify -f allowed -I releases@acs -n acs-release \
    -s SHA256SUMS.sig < SHA256SUMS
```

### How the upgrade replaces the binary

acs replaces itself in place (a link like `~/.local/bin/acs` is pointed at
the new version); if its directory is not yours, it says to use
`sudo acs upgrade`. It uses `curl` (or `wget`). Remote hosts need nothing:
the next connection installs the new version there. An acs installed with
Homebrew is brew's to replace: `acs upgrade` says to run
`brew upgrade acs` instead.

Once a week acs looks for a newer release in the background (it never
delays connecting) and, if there is one, says so once when you next start
it:

```text
acs: acs 0.3.0 is available (you have 0.2.0) — run: acs upgrade
```

(with Homebrew, `run: brew upgrade acs`). Offline, it says nothing. Turn it
off with `ACS_NO_UPDATE_CHECK=1` or `update_check: false` in the
configuration.

Nothing is needed on the remote beyond ssh, a POSIX `sh` and `gzip`: on first
contact acs installs its own version under `~/.local/share/acs/<version>/`
and links `~/.local/bin/acs` to it.

## Use

```text
acs [ssh options] [user@]host            pick a detached session, or create one
acs [ssh options] [user@]host session    attach, or create
acs [ssh options] [user@]host --new      create a new numbered session (1, 2, …)
acs list [ssh options] [user@]host       list sessions on host
acs list [ssh options]                   list sessions on every host alias
acs host session -- command args…        run a command instead of the login shell
```

`list`, `config` and `upgrade` are commands when they come first. A host
with one of those names is reached as `user@list`, with an option before
it (`acs -p 22 list`), or listed as `acs list list`.

A plain `acs host` looks at the host's sessions first. With none
detached, it creates one — `main`, or the lowest free number if `main` is
taken. With some detached, it shows them:

```text
acs: detached sessions on devbox

     NAME  STATE     WHO           IDLE  AGE  COMMAND
> 1  main  detached  (michel@mbp)  4m    1d   /bin/zsh -l
  2  work  detached  (michel@mbp)  2h    3d   htop
  n  new session
     exit

1-9, or ↑↓ jk and Enter: attach   .: all   x: end   n: new   Esc: leave
```

| Key | Effect |
| --- | --- |
| `1`–`9` | attach that session (more than nine: the rest by cursor) |
| ↑ ↓ or `k` `j`, then Enter | attach the session under the cursor, or pick *new session* or *exit* |
| `n` | create a new session (on a session row, or on *new session*) |
| `.` | show attached sessions too; taking one over asks first (`--force` does not) |
| `x` | end the session under the cursor, after a `y` (or a second `x`) |
| Esc | leave the menu (Ctrl-C too) |

The bar names only the keys that act on the row under the cursor, so `x`
and `n` are not offered on *new session* or *exit*, and Enter is named for
what it does there:

```text
1-9: attach   ↑↓ jk: move   .: all   Enter or Esc: leave
```

The list, the menu's `x` and the attach share one ssh connection, so a
plain `acs host` logs in once (one touch of a hardware key), and `acs -v
host` shows a single `running ssh …`. `-v` also times each phase of a
connection — ssh spawned, acs ready on the remote, the session list, the
attach, the first output — as `acs: timing: …` lines, for every redial
too. Without a terminal on stdout there is no menu: `acs host` attaches
`main`, creating it if needed.

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
meaning; `-L` (below) is the exception and goes to the session's
connection alone. There is no `-l`, neither ssh's login option nor a list
option: put a login name in `user@host` or `-o User=`, and list with
`acs list`.
Your `~/.ssh/config`, agent and keys apply as usual; nothing needs
configuring on either side. A key can also be set per
host alias in the configuration (`identity_file`, below); `-i` on the
command line wins.

### Forwarding a port

`-L` forwards a local port over the session's connection, spelled as ssh
spells it and repeatable:

```sh
acs -L 8080:localhost:80 -L 5432:db.internal:5432 devbox
```

`[bind_address:]port:host:hostport`, with an IPv6 literal in brackets
(`-L '[::1]:8080:localhost:80'`) and an empty bind address or `*` for every
interface. A spec acs cannot read is an error before any ssh runs, rather
than a complaint from ssh on every dial.

The forward rides the session's ssh **only** — not the connections
`acs list`, the remote install or the menu over every alias make, which run
side by side and would each try to bind the same port. It follows the
session across reconnects, but it is gone for as long as the link is: the
listening socket belongs to the ssh process, so a drop closes it and the
redial rebinds it, and anything connected through it at the time is cut.
If the port cannot be rebound — a second acs took it, or it is still in
`TIME_WAIT` — ssh says so and the session carries on without the forward;
add `-o ExitOnForwardFailure=yes` if you would rather acs kept redialling
until the port is free. There is no key for adding or removing a forward
once a session is running.

`-o LocalForward="8080 localhost:80"` still works too, and is the way to
forward a unix socket, but it goes to *every* ssh call acs makes (so
several may fight over one port) and is passed to ssh unchecked.

### When the network drops

The client notices a dead link within 10 seconds, redials with backoff
(1 s doubling to 30 s, or at once when your network changes) and resumes
exactly where the output stopped — nothing is lost or repeated. While it is
disconnected a status line shows at the bottom of the screen, typed keys are
dropped, and Ctrl-] Ctrl-] `d` still detaches. `--no-reconnect` exits
instead.

### Never giving up on a host

acs gives up (exit 255) where there is no session to keep: the host
cannot be reached at the start, or the link drops before the session was
attached. With `--persist` (or `persist: true` in the configuration, or
`ACS_PERSIST=1`) it never gives up on a host: it pings it every 5 seconds
(`reachability_interval`) and dials as soon as it answers — at the start as
after a drop, where the ping replaces the backoff. It pings an alias's
hosts as it does to choose one, and a plain host itself; an alias whose
hosts all have `reachability_check: false` cannot be pinged and keeps the
backoff. Before the session exists, Ctrl-C gives up; after, Ctrl-] Ctrl-]
`d` detaches as usual. `persist` can be set globally, on an alias, or on
one of its hosts (the host's wins over the alias's, which wins over the
global one); `--persist` wins over all of them.

### Ctrl-L after reconnecting

Whenever acs attaches to a session that was already running — resuming
after a drop, `acs host session` again after a detach, or taking a session
over — it sends the program one **Ctrl-L** first, so a shell or full-screen
program repaints the screen. A session acs has just created gets none. The
Ctrl-L comes after any keys the drop left unsent and before anything you
type next, reaches the program once per reconnect, and is never sent into
the middle of a paste.

Ctrl-L is a key, not a terminal command: a shell clears the screen, `vim`,
`less` and `htop` redraw, but a program reading raw input receives a form
feed, and `vim` in Insert mode inserts one. Turn it off with
`redraw_on_reconnect: false` in the configuration, for everything or for one
host alias (below), or with `ACS_REDRAW_ON_RECONNECT=0`. The screen is still
cleared and the program still asked to redraw on a re-attach, as before.

### Several people, one account

Each client has an identity (`user@hostname`, or `ACS_IDENTITY`). Attaching
to a session someone else is attached to asks first:

```text
acs: session 'main' on devbox is attached from alice@laptop since 10:02 — take over? [y/N]
```

`--force` skips the question, for that attach. Agreeing to take a session is
about whoever holds it at the time, so it is not reused: if the link later
drops and the redial finds somebody else attached, acs asks again.
`ACS_DEFAULT_SESSION` gives each person their own name instead of `main` for
the session a plain `acs host` creates.

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
redraw_on_reconnect: true  # Ctrl-L after reconnecting (default: true)
reachability_timeout: 1s   # how long hosts have to answer a ping (default: 500ms)
aliases:
  devbox:                  # acs devbox
    - host: devbox.lan     # at home: used if it answers a ping
    - host: devbox.example.com
      user: michel         # from outside, as michel
  nas:
    - host: nas.lan
      reachability_check: false   # it drops pings: use it unchecked
  lab:                     # an alias with settings of its own
    identity_file: ~/.ssh/id_lab  # the key for every host below…
    redraw_on_reconnect: false    # no Ctrl-L after reconnecting to lab
    reachability_timeout: 2s      # a slow link: wait longer for its pings
    hosts:
      - host: lab.lan
      - host: lab.example.com
        identity_file: ~/.ssh/id_lab_outside   # …but this one
```

An alias has one of two shapes, and the example uses both. Usually it is
just its **list of hosts**, as `devbox` and `nas` are. When it has settings
of its own, as `lab` has, it is a **mapping** of those settings, with the
list under `hosts:` — YAML cannot make one node both a list and a mapping.
The mapping form is always allowed, even without settings, and
`acs config host set <alias> <setting> <value>` rewrites a list-form alias
into it (`host unset` of its last setting turns it back into a list).

| Setting | Meaning |
| --- | --- |
| `install_on_remote` | install acs on a host that lacks it (default `true`); when `false`, acs says what is missing and exits with 254 |
| `update_check` | look for a newer acs release once a week (default `true`) |
| `command_bell` | ring the terminal bell when Ctrl-] Ctrl-] arms command mode (default `true`; `ACS_COMMAND_BELL` overrides it) |
| `redraw_on_reconnect` | send Ctrl-L after reconnecting to a session (default `true`); an alias's own value wins, and `ACS_REDRAW_ON_RECONNECT` over both |
| `reachability_timeout` | how long an alias's hosts have to answer a ping: `500ms`, `0.5s`, `2s`, up to `60s` (default `500ms`); an alias's own value wins |
| `persist` | never give up on a lost host: ping it and dial when it answers (default `false`); also on an alias or one of its hosts, the most specific winning; `--persist` and `ACS_PERSIST` over all |
| `reachability_interval` | how often a lost host is pinged while persisting: `100ms` to `3600s` (default `5s`); an alias's own value wins |
| `prefer_local_network` | try first an alias's hosts that are on a network this machine is on, IPv4 or IPv6 (default `false`); an alias's own value wins |
| `aliases` | each alias name maps to a list of `host` entries, with an optional `user`, `identity_file`, `reachability_check` (default `true`), `prefer`, `persist` and `local_networks` — or to a mapping of the alias's own settings (`identity_file`, `redraw_on_reconnect`, `reachability_timeout`, `persist`, `reachability_interval`, `prefer_local_network`) and its `hosts` |

A mistake in a file stops acs with the file and line, for example
`~/.config/acs/config.yaml:2: install_on_remote: expected true or false`.

### Host aliases

`acs devbox` with the file above uses `devbox.lan` if it answers a ping,
otherwise `michel@devbox.example.com` if that one does. An entry with
`reachability_check: false` is used without a ping. An IPv6 host is pinged
with `ping6` where the system's `ping` cannot reach one (macOS). The hosts are pinged
all at once, and the order in the file still decides: an earlier host that
answers within `reachability_timeout` wins over a later one that answered
first, so choosing takes at most that long however many hosts there are.
`prefer: true` on an entry puts it ahead of the others: when it answers it
is used even if an earlier host answered too (several preferred entries go
by order among themselves), which also lets your own file name the primary
host of an alias whose other hosts are in the global file. With
`prefer_local_network: true`, a host whose address is on one of this
machine's networks goes first of all: at home on 192.168.1.0/24,
`devbox.lan` (192.168.1.20) is used before `devbox.example.com` wherever
it is listed — as long as it answers its ping.

An interface's own prefix is narrower than a site, so that match alone
misses a host one subnet away: from 172.16.1.65/24 a host at 172.16.8.2 is
on another network, although both are at the same site. `local_networks`
on a **host entry** says where that entry is the one to use — *when this
machine is on one of these networks, try this host first*:

```yaml
aliases:
  devbox:
    - host: devbox.example.com
    - host: devbox.lan
      local_networks: [172.16.0.0/16, 2001:db8:1::/48]
```

The address tested is **this machine's own**, not the host's: at the site,
on 172.16.1.65, `devbox.lan` goes first; from a café on 10.0.0.0/8 it does
not, even though its own address is still inside 172.16.0.0/16. That is
what makes the setting say something about *where you are*, and it is why
it belongs to one entry rather than to the alias or the whole file —
"prefer this host when I am on network X" does not parse without naming
the host. It needs no `prefer_local_network` (it costs no name lookup, so
there is nothing to switch off) and it does not exempt the host from its
ping; it only ranks it. Several entries may carry the setting — among
those that match, the order in the file decides — and an entry may be
local both ways at once, in which case `-v` names the network the host
itself is on. A network that is not in CIDR form, matches every address
(`/0`), or is loopback or link-local is a configuration error naming the
file and line.

Without a
`user`, your `~/.ssh/config` picks the login name. If no entry
answers, acs lists the hosts it tried and exits with 255. `-v` accounts
for **every** entry of the alias, one line each: which was chosen and why,
which did not answer its ping, and — the entries the choice never reached
— which host was chosen before them, or which `reachability_check: false`
entry they are listed behind and so could never have been used:

```text
acs: devbox: devbox.lan does not answer ping within 500ms
acs: devbox: devbox.vpn answers ping, using devbox.vpn
acs: devbox: devbox.backup not tried: devbox.vpn was chosen first
acs: devbox: devbox.old not tried: it is listed after devbox.backup, whose reachability_check is off
```

The untried entries come last, continuing the order the choice walked in.

The alias is resolved again on every reconnect, so when you move from home
to outside, the redial goes to whichever address answers. List ways of
reaching **one** machine under an alias: the session lives on that machine,
so a fallback to a different one finds no session to resume.

`acs root@devbox` goes through the alias the same way, logging in as `root`
on whichever host is chosen (instead of `michel` on the fallback), and keeps
`root` on every redial. Any name that is not an alias, with or without a
`user@`, is used as given. To reach a machine whose name is also an alias,
use its full name or address (`acs devbox.example.com`).

### Which ssh key

`identity_file` picks the key acs passes to ssh as `-i`, for one host entry
or for a whole alias. The first of these wins, and only that one is passed:

1. `-i` (or `-o IdentityFile=`) on the command line;
2. the chosen host entry's `identity_file`;
3. the alias's `identity_file`.

With none of them, ssh picks the key as usual (`~/.ssh/config`, the agent).
A leading `~/` means your home directory. The key follows the entry: a
reconnect that falls back to another host of the alias uses that host's
key. ssh still offers its other keys after this one unless you set
`IdentitiesOnly yes`. Your own file can set just the key of an alias whose
hosts are in the global file (`lab: {identity_file: ~/.ssh/mine}`).

### Sessions on every host

`acs list` without a host shows the sessions on every alias at once. In a
terminal it is the session menu above, with a HOST column: pick a session
on any host with its number or the cursor, `x` ends one on its host, `n`
starts a new one on the host of the row under the cursor, and each host's
rows appear as it answers. `acs list <alias>` is that one host's menu.
Into a pipe (`acs list | less`) it is a table:

```text
HOST    NAME  STATE     WHO           IDLE  AGE  COMMAND
devbox  main  attached  michel@mbp    3s    2h   /bin/zsh -l
devbox  work  detached  (michel@mbp)  4m    1d   htop
no sessions on nas
acs: pi: no host for 'pi' is reachable (tried pi.lan)
```

Each alias is resolved as above and all are asked in parallel, so a host
that is down or slow only costs its own line — on stderr, after at most 30 s
(`ACS_DIAL_TIMEOUT_MS`). The exit status is 0 when every host answered and
255 when any did not. Several ssh cannot ask for passwords on one terminal,
so these calls run with ssh's `BatchMode`: list a host that needs a password
on its own, with `acs list <alias>`. With no aliases configured, it says so
— or, with no configuration file at all, that there is none and where it
looked — and exits with 2.

### Editing it from the command line

```sh
acs config host add devbox devbox.lan                 # the first host of devbox
acs config host add devbox devbox.example.com --user michel
acs config host add nas nas.lan --no-reachability-check
acs config host add lab lab.lan --identity-file ~/.ssh/id_lab_home
acs config host set lab identity_file ~/.ssh/id_lab   # the alias's key
acs config host unset lab identity_file
acs config host set lab redraw_on_reconnect false     # no Ctrl-L for lab
acs config host set lab reachability_timeout 2s       # lab's hosts may take 2 s
acs config host set lab persist true                  # never give up on lab
acs config host add nas nas.lan --persist             # nor on this host of nas
acs config host add devbox devbox.vpn --prefer        # devbox.vpn first when it answers
acs config host list                                  # aliases, hosts and keys
acs config host remove devbox devbox.lan              # one host, or the alias
acs config set install_on_remote false
acs config set update_check false                     # no weekly release check
acs config set command_bell false                     # no bell for command mode
acs config set redraw_on_reconnect false              # no Ctrl-L on reconnect
acs config set reachability_interval 10s              # ping a lost host every 10 s
acs config host set devbox prefer_local_network true  # devbox.lan first at home
acs config host add devbox devbox.site --local-networks 172.16.0.0/16
                                                      # first when I am on that
acs config set reachability_timeout 250ms             # pings must answer within 250 ms
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
| `ACS_DEFAULT_SESSION` | name of the session plain `acs host` creates, and attaches without a terminal (default `main`) |
| `ACS_IDENTITY` | identity shown to others on a shared account |
| `ACS_ESCAPE_KEY` | command key in `^X` notation (default `^]`) |
| `ACS_ESCAPE_TIMEOUT_MS` | window for the double press (default 400) |
| `ACS_COMMAND_BELL` | `0`: no bell when command mode arms; `1`: a bell even if the configuration turns it off |
| `ACS_REDRAW_ON_RECONNECT` | `0`: no Ctrl-L after reconnecting; `1`: a Ctrl-L even if the configuration (global or the alias's) turns it off |
| `ACS_PERSIST` | `1`: never give up on a lost host (as `--persist`); `0`: give up as by default, whatever the configuration says |
| `ACS_SSH` | ssh program (default `ssh`; also `--ssh`) |
| `ACS_SOCKET_DIR` | remote socket directory (default `/tmp/acs-<uid>`) |
| `ACS_RING` | remote output history kept for resume, bytes (default 1 MiB) |
| `ACS_DIAL_TIMEOUT_MS` | how long a connection may take to answer (default 120 s at first, 30 s on a redial and for each host of `acs list`) |
| `XDG_CONFIG_HOME` | where your configuration file is (default `~/.config`) |
| `ACS_GLOBAL_CONFIG` | global configuration file (default `/etc/acs/config.yaml`) |
| `ACS_PING` | ping program for alias reachability checks (default `ping`) |
| `ACS_RELEASES_URL` | where `acs upgrade` and the update check look for releases (default GitHub). Must be `https`, since what is fetched is checked only against sums from the same place and is then run; `acs upgrade --allow-insecure-url` accepts another scheme. Ignored altogether when the real and effective user differ, so a `sudo` upgrade is not steered by the environment |
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
scripts/update-tap.sh vX.Y.Z # point the Homebrew tap at a release
```

`scripts/e2e_ssh.sh --no-build` reuses the binaries already in `dist/` (about
20 s instead of a minute), but only when `dist/source.stamp` says they were
built from the tree as it is now — otherwise it refuses, since a failure from
a stale binary looks exactly like a real one. `--allow-stale-dist` runs it
anyway.

Results of the checks that need a real terminal are in
[docs/VERIFICATION.md](docs/VERIFICATION.md).
