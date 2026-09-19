# Versioning

**Status:** Built — `scripts/version-bump.sh` (`cargo xtask bump`),
`scripts/release-binaries.sh` (`cargo xtask package`) and
`scripts/update-tap.sh` (`cargo xtask formula`).

acs follows semantic versioning, and the version moves **automatically** after
every merge to `main`. The version lives in `Cargo.toml` (and so in
`Cargo.lock`, in `acs --version`, and in the remote install path
`~/.local/share/acs/<version>/`); each release is an annotated tag `vX.Y.Z` on
a `chore(release): vX.Y.Z` commit.

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

Each squash commit on `main` since the last `vX.Y.Z` tag is classified; the
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
- The `ship` workflow runs it after every merge, then pulls again.
- The first run on a repository without tags releases the current
  `Cargo.toml` version as it is.

## Binaries

Every release is published on
[GitHub Releases](https://github.com/michel-onstein/acs/releases) with:

- `acs-<version>-<target>.tar.gz` for `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` — the complete builds, each holding
  `acs-<version>-<target>/acs` and the README;
- `SHA256SUMS` over the archives;
- `install.sh`, the one-line installer (`scripts/install.sh` at the tag), so
  `…/releases/latest/download/install.sh` always names the newest one;
- notes with install instructions and the changes since the previous release.

`scripts/version-bump.sh` publishes them right after it tags a release;
`scripts/release-binaries.sh [vX.Y.Z]` does it on its own (for example to
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
- `scripts/update-tap.sh vX.Y.Z` renders and pushes that release's formula
  from its published `SHA256SUMS` — the way to retry if the push failed
  after the release was published. `ACS_TAP_REPO` points it at another
  repository (the tests use a local one).

The formula is generated: change `xtask/src/formula.rs`, not the tap. A
Homebrew install is brew's to replace — its real path is in a `Cellar` —
so `acs upgrade` refuses and says `brew upgrade acs`, and so does the weekly
update message (DESIGN §7.5, §7.6).

## Options

Options: `--minor` / `--patch` force a level; `--large-lines N`;
`--no-labels` skips the pull request lookup (which uses `gh`); `--remote`,
`--branch` and `--repo` point it elsewhere.
