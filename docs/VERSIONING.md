# Versioning

**Status:** Built — `scripts/version-bump.sh` (`cargo xtask bump`),
`scripts/release-binaries.sh` (`cargo xtask package`) and
`scripts/update-tap.sh` (`cargo xtask formula`). The fork knobs ("Forking")
are all built: a fork publishes a complete release — binaries, packaged
installer, Homebrew formula and notes — without editing a tracked source
file (acs-x57). Release tags are bare from 0.19.0 on, with v0.18.0 the
bridge ("Tags", acs-aycl).

acs follows semantic versioning, and the version moves after every merge to
`main` — when whoever landed it runs the bump ("Who runs it"). The version
lives in `Cargo.toml` (and so in
`Cargo.lock`, in `acs --version`, and in the remote install path
`~/.local/share/acs/<version>/`); each release is an annotated tag `X.Y.Z` on
a `chore(release): X.Y.Z` commit — releases up to 0.18.0 were tagged
`vX.Y.Z` ("Tags").

## Tags

From 0.19.0 on a release's tag is the bare version, `0.19.0`; every release
up to and including 0.18.0 is tagged `v0.18.0` and keeps that tag. The
switch (acs-aycl, 2026-10-29) went through a **bridge release**:

- Installed acs up to 0.17.0 fetches a named release and every archive from
  `…/releases/download/v<version>/` (`acs upgrade`; the weekly update check
  reads only `…/latest/download/SHA256SUMS`). An archive published under a
  bare tag is unreachable from them by that path.
- So **v0.18.0 was published with its `v`**: they upgrade to it as to any
  release, and 0.18.0 knows both forms. From 0.19.0 the bump tags bare.
- Clients map a version to its tag — `vX.Y.Z` up to 0.18.0, `X.Y.Z` after —
  in one place per language: `release::release_tag` with the cutoff
  `release::LAST_V_TAG` (acs and xtask's formula and notes), and
  `release_tag` with `last_v_tag` in `scripts/install.sh`
  (`ACS_VERSION=0.17.0` still fetches `v0.17.0`). Either spelling of the
  version is accepted (`acs upgrade --version v0.19.0`). The
  `…/latest/download/` paths do not name a tag and are unaffected.
- Everything that lists or parses tags takes both forms and orders them as
  one sequence by version: the bump's previous tag, `scripts/release-tags.sh`
  (newest first) and through it `scripts/release-binaries.sh`'s latest and
  `scripts/release-changes.sh`'s previous — so 0.19.0's notes list the
  changes since v0.18.0 — and the tags `scripts/version-bump.sh` publishes.
  `git tag --sort=-v:refname` would not do: it compares the `v` as text.
- `scripts/release-binaries.sh v0.18.0` and `scripts/update-tap.sh v0.18.0`
  still work from a checkout that tags bare, rendering URLs under
  `v0.18.0`.

## The rules

| Change | Level | Example |
| --- | --- | --- |
| A small fix | **PATCH** — `0.4.2` → `0.4.3` | `fix: reset mouse mode on detach` |
| A larger fix | **MINOR** — `0.4.2` → `0.5.0` | a fix changing ≥ 300 lines of shipped code, or marked `Semver: minor` |
| A new feature | **MINOR** | `feat: add --new` |
| A breaking change | **MINOR** (never MAJOR on its own) | `feat!: …`, `BREAKING CHANGE:` in the body |
| Docs, tests, beads, scripts, CI | none | `docs: …`, `test: …`, or a commit touching no shipped code |
| A new major version | **MAJOR** — `0.4.2` → `1.0.0` | only by explicit request: `scripts/version-bump.sh --major` |

MAJOR resets MINOR and PATCH to 0. Nothing but `--major` produces it.

### How a commit is read

Each squash commit on `main` since the last release tag is classified; the
highest level among them is the release:

1. A **`Semver: minor|patch|none` trailer** in the commit message, or a
   **`semver:minor|patch|none` label** on its pull request, decides outright.
   (`major` there counts as `minor`: MAJOR only happens on request.)
2. Otherwise a **Conventional Commits** subject decides: `feat` → MINOR;
   `fix`, `perf`, `refactor`, `revert` → PATCH; `docs`, `chore`, `test`, `ci`,
   `style`, `build` → none; a breaking change → MINOR.
3. A subject without a conventional prefix → PATCH if it changes shipped code,
   none otherwise.
4. **Larger fix:** a PATCH commit that changes 300 or more lines of shipped
   code counts as MINOR (`--large-lines N` moves the threshold).

**Shipped code** is `src/`, `build.rs`, `Cargo.toml` and `Cargo.lock`;
everything else (docs, tests, `xtask/`, `scripts/`, `.beads/`) changes nothing
a user runs.

So: write pull request titles in Conventional Commits form (`fix: …`,
`feat: …`) — the squash commit takes the title — and mark a small-looking fix
that deserves a minor release with a `Semver: minor` trailer or label.

## Running it

```sh
scripts/version-bump.sh --dry-run   # show each commit's level and the release
scripts/version-bump.sh             # release: commit, tag, push to origin/main
scripts/version-bump.sh --major     # a new major version (manual only)
```

- It reads everything unreleased since the last tag, not only the last
  commit, so a chore landing after a feature does not hide the feature.
- It is safe to re-run: with nothing unreleased it does nothing.
- It fetches origin's tags before noting which ones exist, so only the tag it
  made is published — a checkout whose tags were stale does not publish the
  releases it was missing all over again.
- It never touches your checkout: it releases from a throwaway worktree of
  `origin/main` and pushes the commit and tag atomically. Afterwards update
  your `main` with `git pull --ff-only`.
- The `ship` skill runs it after every merge, then pulls again.
- The first run on a repository without tags releases the current
  `Cargo.toml` version as it is.

### Who runs it: there is no CI

The repository has **no CI** — no `.github/`, no GitHub Actions, nothing
that runs on a push, a merge or a schedule. `gh pr checks` saying "no checks
reported" is the expected answer, not a broken pipeline.

- **`scripts/verify.sh` is the gate.** It runs where the person landing the
  change runs it, on their machine, before the merge; the container, ssh and
  installer suites (`scripts/test_linux.sh`, `scripts/e2e_ssh.sh`,
  `scripts/test_install.sh`) likewise run only when someone runs them.
- **The version bump is a local step, not an automated one.** The `ship`
  skill runs `scripts/version-bump.sh` at its version-bump step, after the
  merge and on the lander's machine. If nobody runs `ship`, or it is told to
  skip that step, nothing bumps: `main` simply sits ahead of its last tag,
  with its changes unreleased, until someone runs the script. That is safe —
  the next run releases everything since the tag ("Running it").

## Binaries

Every release is published on
[GitHub Releases](https://github.com/michel-onstein/acs/releases) with:

- `acs-<version>-<target>.tar.gz` for `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` — the complete builds, each holding
  `acs-<version>-<target>/acs` and the README;
- `SHA256SUMS` over the archives;
- `install.sh`, the one-line installer (`scripts/install.sh` at the tag, with
  this build's releases URL and release key substituted in — "The packaged
  installer"), so `…/releases/latest/download/install.sh` always names the
  newest one;
- notes with install instructions and the changes since the previous release.

### What the notes list as the changes

`scripts/release-changes.sh [TAG]` prints that list on its own — one line
per pull request merged since the previous release tag, newest first, taken
from the first-parent log. Run it to see what a release's notes will say.

The bookkeeping is left out, because the notes are read by someone about to
install the binary and it tells them nothing about it:

- `chore(release):` — the version bump that *is* this release;
- `chore(beads):` — the commits that only open and close issues under
  `.beads/`. Together they are the majority: of the 51 pull requests in
  v0.17.0, 26 were one of these two and 25 were changes to acs.

Nothing else is filtered. A `chore` that touches the build, the scripts or
the packaging is a change someone installing may care about, so it stays. A
release that is *only* bookkeeping lists no changes at all rather than
failing.

`scripts/version-bump.sh` publishes them right after it tags a release;
`scripts/release-binaries.sh [TAG]` does it on its own (for example to
publish an existing tag again: it replaces the assets). It builds from the
exact tag in a throwaway worktree, needs `gh` with access to the repository
(`GH_TOKEN` works), `cargo-zigbuild` and `zig`, and `--dry-run` builds and
packages without uploading. `ACS_NO_PUBLISH=1` makes the bump skip it.

## Homebrew

The tap [michel-onstein/homebrew-acs](https://github.com/michel-onstein/homebrew-acs)
holds one formula, `Formula/acs.rb`, so `brew install michel-onstein/acs/acs`
installs acs on macOS and on Homebrew for Linux. It installs the **release
archive** for the machine (`on_macos` / `on_linux`, `on_arm` / `on_intel`,
each with its SHA-256 from `SHA256SUMS`) rather than building from source: the
release binaries are the complete builds, which install acs on any Linux
remote (DESIGN §8.1); a `cargo build` from source would be slim. Its test runs
`acs --version` and checks the version and that the Linux payloads are there.

The release keeps it current. After publishing, `scripts/release-binaries.sh`
renders the formula from the release's `SHA256SUMS`
(`cargo xtask formula --version X.Y.Z --sums SHA256SUMS --out acs.rb`) and
`scripts/update-tap.sh` commits it to the tap as `acs X.Y.Z` and pushes:

- it clones the tap into a temporary directory and pushes with `gh`'s token
  (`GH_TOKEN` works), set as the credential helper of that clone only;
- a formula that is already there does nothing, and one older than the tap's
  (an old tag published again) is left out, so the tap never moves back;
- `--dry-run` shows the change without committing; the release's own
  `--dry-run` runs it that way, and `ACS_NO_TAP=1` skips the tap altogether;
- `scripts/update-tap.sh TAG` renders and pushes that release's formula
  from its published `SHA256SUMS` — the way to retry if the push failed
  after the release was published. `ACS_TAP_REPO` points it at another
  repository (the tests use a local one) and `ACS_RELEASES_URL` at another
  release channel to read that `SHA256SUMS` from; by default it is the
  build's channel.
- before rendering anything from a published `SHA256SUMS`, it checks
  `SHA256SUMS.sig` with `ssh-keygen -Y verify`, as the installer does, and
  refuses a release that is unsigned or not signed by the build's release
  key (acs-oxl4). Homebrew users never see the signature — brew checks only
  the sha256s the formula pins — so this is the check they get. The key is
  the one `cargo xtask channel` reports, xtask's own `RELEASE_KEY`, which is
  what the formula and the packaged installer carry: a fork that builds with
  `ACS_DEFAULT_RELEASE_KEY` ("Forking") checks against its own key, and no
  variable names another.

The formula's homepage and download URLs are the build's
`DEFAULT_RELEASES_URL` ("Forking"), so a fork's tap points at the fork. To
decide whether the tap would move backwards, `scripts/update-tap.sh` reads the
release from the `# acs-version: X.Y.Z` line the generator writes into the
formula's header, not from those URLs — so a fork whose releases are not
GitHub-shaped gets a formula the tap takes (acs-u3qn). A formula without that
line is refused; one already on the tap without it (rendered before the line
existed) counts as no version and is replaced.

The formula is generated: change `xtask/src/formula.rs`, not the tap. A
Homebrew install is brew's to replace — its real path is in a `Cellar` —
so `acs upgrade` refuses and says `brew upgrade acs`, and so does the weekly
update message (DESIGN §7.5, §7.6).

## Forking

A fork that publishes its own releases points **three** things at itself, and
**edits no tracked source file**. Two are decided when the binary is built and
belong together; the third is the tap.

| Knob | Reaches | How |
| --- | --- | --- |
| The releases a binary updates from | `release::DEFAULT_RELEASES_URL`, the packaged `install.sh`, the formula's homepage and download URLs, the notes' install instructions | build with `ACS_DEFAULT_RELEASES_URL=https://github.com/you/acs/releases` |
| The release signing key | `signature::RELEASE_KEY`, the packaged `install.sh`, the formula's comment header | build with `ACS_DEFAULT_RELEASE_KEY="$(cat release_key.pub)"` |
| The tap that is pushed | `scripts/update-tap.sh`, the notes' `brew install` line | `ACS_TAP_REPO=https://github.com/you/homebrew-acs.git`, or `ACS_NO_TAP=1` for no tap at all |

```sh
export ACS_DEFAULT_RELEASES_URL=https://github.com/you/acs/releases
export ACS_DEFAULT_RELEASE_KEY="$(cat release_key.pub)"
export ACS_TAP_REPO=https://github.com/you/homebrew-acs.git
scripts/release-binaries.sh
```

**Set them for the whole release, not for one step.** `cargo xtask package`
and `cargo xtask formula` read the same two build-time values as the
binaries — xtask links the `acs` library, so `build.rs` gives it the same
`DEFAULT_RELEASES_URL` and `RELEASE_KEY` — which is what lets the packaged
installer, the formula and the notes follow a fork with no edit. A `dist`
built with the variables and a `package` run without them would produce
binaries that update from the fork beside an installer that installs
upstream, so that is refused: `dist` records the releases URL and key it
built for in `dist/channel.stamp`, and `package` refuses a `dist/` whose
stamp differs from its own — or that has none — before writing anything
(acs-u3qn). `dist` likewise refuses to run when the environment would build
for a different channel than the xtask it is (a cached xtask binary).
`scripts/release-binaries.sh` runs every step in one environment, and
`package` prints the URL and key it substituted.

The build-time URL is what `acs upgrade` and the weekly update check look at
when nothing overrides them, and the one-line installer named in the "acs is
not installed on that host" message is derived from it, so a fork's users are
not sent upstream to install. Unset — every build of acs itself — it is
`https://github.com/michel-onstein/acs/releases`, and an empty value counts as
unset. A value that is not one https URL with a host — `http://`, no scheme,
an empty or malformed host or port, a space or line break, a query or
fragment, credentials — **fails the build**, the same way a malformed key
does, rather than shipping a binary whose update channel is silently
unreachable (acs-d7x). Past that it is taken as given: whoever builds the
binary already chooses what it does, and no build can tell whether the
server answers. The runtime `ACS_RELEASES_URL` override is a
different matter and keeps its guards — https only unless
`--allow-insecure-url` is passed on the command line, and ignored outright
when the real and effective user differ (DESIGN §7.5).

The signing key travels with the URL, which is why it is set in the same
build: `signature::key_for` uses the built-in `RELEASE_KEY` for whatever
`DEFAULT_RELEASES_URL` names, and nothing at runtime can replace it there. A
fork that bakes in its own releases URL but keeps acs's key cannot verify its
own releases, and `acs upgrade` will refuse them. The value is the public
half — one line, the contents of an `ssh-keygen` `.pub` file; the private
half signs `SHA256SUMS` (`ACS_SIGNING_KEY` in `scripts/release-binaries.sh`)
and stays off every machine that only runs acs. The key also goes into the
generated Homebrew formula, so a fork's tap is a second source for the fork's
key as acs's tap is for acs's.

Neither knob can leave a binary that verifies nothing. Unset — every build of
acs itself — the key is acs's own, an empty value counts as unset, and a
value that is not an ssh public key line **fails the build** rather than
shipping a release that no one can install. The body is decoded, not just
looked at: it must be base64 whose blob names the key type the line declares
and is made of whole fields, so a truncated paste or an `ssh-rsa` line with
an ed25519 body is a build error too (acs-d7x). A key that is merely the wrong
one fails closed: every release is refused, nothing is downloaded or run.
Like the URL, it is the builder's decision rather than the environment's, so
it carries none of the limits on the runtime `ACS_RELEASE_KEY` override —
which is read only for a channel that has itself been redirected, and is
untouched here (DESIGN §7.5).

### The packaged installer

`scripts/install.sh` in the repository carries the upstream releases URL and
acs's own key — it is upstream's installer, and someone who fetches it from
here gets upstream's acs. The copy published as a release asset is **not**
that file: `cargo xtask package` substitutes the build's
`DEFAULT_RELEASES_URL` and `RELEASE_KEY` into the two assignments it names,
so a fork's `…/releases/latest/download/install.sh` installs the fork's acs
and checks the fork's signature (acs-x57).

- Each value is written as **one single-quoted assignment at the start of a
  line**. Single quotes are what make the substitution safe: `sh` expands
  nothing inside them, so a build-time value becomes exactly that string and
  can never become a command, whatever is in it. A value with a line break
  is refused, because it could not be read back.
- If either line ever moves, or comes to be written twice, **packaging
  fails** rather than publishing an installer that quietly kept acs's key.
  `package` also reads both values back out of what it is about to write and
  compares them with what it meant to put there.
- Two tests hold the ends together:
  `scripts::the_installer_carries_the_same_release_key_and_checks_before_reading`
  keeps the checked-in script on acs's own URL and key, and
  `package::the_packaged_installer_carries_this_builds_url_and_key` holds the
  packaged copy against `RELEASE_KEY` and `DEFAULT_RELEASES_URL` for every
  build, a fork's included.

**There is deliberately no `ACS_RELEASE_KEY` for the installer**, although
`ACS_RELEASES_URL` is honoured (https only, ignored under sudo) and acs
itself takes a key override. It is not an oversight, and the substitution
does not change the answer:

- acs reads `ACS_RELEASE_KEY` **only for a channel `ACS_RELEASES_URL` has
  already redirected**, and only because the guards that make that safe exist
  there: https, ignored across a privilege boundary, and `--allow-insecure-url`
  **on the command line** — deliberately not an environment variable, since
  whoever set the URL would set that too.
- The installer has no command line. It is fetched over the network and piped
  into `sh`, often as root, so every knob it could offer is an environment
  variable — exactly the thing acs refused for the key. The one guard it
  could not reproduce is the one that matters.
- It costs nothing. A mirror signed by another key is still installable: the
  fork packages its own installer with its own key baked in, or a user
  downloads the archive and checks `SHA256SUMS` by hand. Without an override
  the installer fails closed — a release signed by another key is refused,
  not installed — and exactly one key is ever in play in a packaged
  installer, which is what lets the drift guard be an equality.

## Options

Options: `--minor` / `--patch` force a level; `--large-lines N`;
`--no-labels` skips the pull request lookup (which uses `gh`); `--remote`,
`--branch` and `--repo` point it elsewhere.
