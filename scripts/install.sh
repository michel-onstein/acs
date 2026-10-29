#!/bin/sh
# Install acs (https://github.com/michel-onstein/acs):
#
#   curl -fsSL https://github.com/michel-onstein/acs/releases/latest/download/install.sh | sh
#
# Picks the release build for this machine (macOS arm64/x86_64, Linux
# x86_64/aarch64), checks it against the release's SHA256SUMS and installs
# it the way acs installs itself on a remote (DESIGN §8):
#
#   per user  ~/.local/share/acs/<version>/acs, linked from ~/.local/bin/acs
#   as root   /usr/local/lib/acs/<version>/acs, linked from /usr/local/bin/acs
#
# so a client of the same version finds it there and has nothing to upload.
# Beside the acs link it makes acscp -> acs (relative), the name acs copies
# files under (ACSCP_DESIGN §9), by the same rule: made where nothing is,
# never over a file or link the user put there, and not beside an acs that
# is the user's own.
# Running it again upgrades in place. The link follows the rule acs keeps
# for ~/.local/bin/acs on a remote (DESIGN §8): it is made where nothing
# is, and moved only when it is a link of ours (into the versioned
# directory) to an older or removed version. So ACS_VERSION=<older>
# installs that version beside a newer one without taking the link back,
# and a file or link the user put there is left alone; either way the
# installer says so, and where the version it installed is.
#
# Environment:
#   ACS_VERSION=X.Y.Z     install that release instead of the latest
#   ACS_INSTALL_DIR=DIR   put the binary itself in DIR (no versioned layout:
#                         DIR/acs is replaced, with DIR/acscp -> acs beside it)
#   ACS_RELEASES_URL=URL  where releases are (default: the GitHub releases).
#                         Must be https unless ACS_ALLOW_INSECURE_URL=1, and
#                         it is ignored when running through sudo (acs-95w).
#   ACS_ALLOW_INSECURE_URL=1  accept a releases URL that is not https
#
# There is deliberately no ACS_RELEASE_KEY (acs-x57). acs itself takes one,
# but only for a channel that ACS_RELEASES_URL has already redirected, and
# only because the guards that make that safe -- https, no privilege
# boundary, --allow-insecure-url on the *command line* -- exist there. This
# script is fetched over the network and piped into sh, often as root: it
# has no command line to put an explicit opt-out on, so every knob it could
# offer is an environment variable, which is what acs refused. It therefore
# checks against the key baked in below, and nothing else; a release signed
# by another key is refused, not installed (docs/VERSIONING.md, "Forking").
set -eu

# `cargo xtask package` rewrites this assignment and the release_key one
# below when it copies this script into a release, so a fork's packaged
# installer points at the fork's releases and carries the fork's key with
# no edit to this file (acs-x57, docs/VERSIONING.md "Forking"). Each must
# stay a single assignment at the start of a line of its own: packaging
# fails rather than shipping a stale value if either moves.
default_releases='https://github.com/michel-onstein/acs/releases'
releases=${ACS_RELEASES_URL:-$default_releases}
# What is fetched from here is checked only against a SHA256SUMS from the
# same place, and is then installed and run. So: not across a privilege
# boundary, and not over a scheme that cannot be authenticated (acs-95w).
if [ -n "${ACS_RELEASES_URL:-}" ] && [ -n "${SUDO_USER:-}" ]; then
    printf 'acs-install: ignoring ACS_RELEASES_URL under sudo\n' >&2
    releases=$default_releases
fi
case "$releases" in
    https://*) ;;
    *)
        if [ "${ACS_ALLOW_INSECURE_URL:-}" != 1 ]; then
            printf 'acs-install: error: %s\n' \
                "ACS_RELEASES_URL is not https, which cannot be authenticated; set ACS_ALLOW_INSECURE_URL=1 to accept it" >&2
            exit 1
        fi
        ;;
esac
want=${ACS_VERSION:-}
want=${want#v}

say() { printf 'acs-install: %s\n' "$*" >&2; }
die() {
    say "error: $*"
    exit 1
}

# ---- what to download --------------------------------------------------------

os=$(uname -s)
arch=$(uname -m)
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] &&
    [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
    arch=arm64 # a shell under Rosetta: the native build is the better one
fi
case "$os/$arch" in
    Darwin/arm64 | Darwin/aarch64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
    *) die "acs has no build for $os $arch (there are builds for macOS arm64 and x86_64, and Linux x86_64 and aarch64)" ;;
esac

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "need curl or wget to download acs"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
elif command -v openssl >/dev/null 2>&1; then
    sha256() { openssl dgst -sha256 "$1" | sed 's/.*= *//'; }
else
    die "need sha256sum, shasum or openssl to check the download"
fi

# The checksums are signed, and the signature is checked before they are
# read (acs-o9v): they and the archives come from the same place, so
# unsigned they prove only that an archive matches what that place said.
# ssh-keygen does the checking -- acs is an ssh tool, so a machine that
# cannot run it cannot run acs -- and the key below is the public half of
# the key that signs acs releases. It is in the binary too, and in the
# Homebrew tap, which is a repository of its own to check it against.
# Rewritten by `cargo xtask package`, like default_releases above.
release_key='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILbxQW5C9X7CdwcQ4bab0gsQi4Evk2xfgmI/972dlHCb acs release signing'
command -v ssh-keygen >/dev/null 2>&1 ||
    die "need ssh-keygen to check the release signature (it comes with ssh, which acs requires)"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/acs-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# The tag a release is published under (docs/VERSIONING.md, "Tags"):
# vX.Y.Z up to $last_v_tag, X.Y.Z after it. Installed acs up to 0.18.0 only
# knows the download/v<version>/ path (0.18.0 was tagged before this
# mapping existed), so 0.19.0, the bridge, is published under v0.19.0 too;
# release::LAST_V_TAG in acs is the same cutoff.
last_v_tag=0.18.0
release_tag() { # release_tag X.Y.Z
    old_ifs=$IFS
    IFS=.
    # shellcheck disable=SC2086 # split on the dots, on purpose
    set -- $1 $last_v_tag
    IFS=$old_ifs
    if [ "$1" -lt "$4" ] ||
        { [ "$1" -eq "$4" ] && { [ "$2" -lt "$5" ] ||
            { [ "$2" -eq "$5" ] && [ "$3" -le "$6" ]; }; }; }; then
        printf 'v%s.%s.%s\n' "$1" "$2" "$3"
    else
        printf '%s.%s.%s\n' "$1" "$2" "$3"
    fi
}

# SHA256SUMS names every archive, so it also tells the latest version.
if [ -n "$want" ]; then
    printf '%s\n' "$want" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' ||
        die "ACS_VERSION wants X.Y.Z, not '$want'"
    base="$releases/download/$(release_tag "$want")"
else
    base="$releases/latest/download"
fi
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "cannot download $base/SHA256SUMS${want:+ (is $want a release?)}"
fetch "$base/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" ||
    die "the release has no signature for SHA256SUMS ($base/SHA256SUMS.sig)"
printf 'releases@acs namespaces="acs-release" %s\n' "$release_key" > "$tmp/allowed_signers"
ssh-keygen -Y verify -f "$tmp/allowed_signers" -I releases@acs -n acs-release \
    -s "$tmp/SHA256SUMS.sig" < "$tmp/SHA256SUMS" >/dev/null 2>&1 ||
    die "the release's SHA256SUMS is not signed by the acs release key; nothing was installed"
# The version is X.Y.Z and nothing else (acs-g3j). The old pattern took
# [^ ]+ for the whole name, which allows / and .., so a SHA256SUMS from a
# channel under someone else's control could name a version that walks out
# of $tmp and out of $lib -- and $lib is written as root in the root
# branch. The Rust side has always been this strict; the two now agree,
# on the separator too: one space, then any run of spaces and `*`s
# (release::sums_line; tests/scripts.rs holds them together).
line=$(grep -E "^[0-9a-f]{64} [ *]*acs-[0-9]+\\.[0-9]+\\.[0-9]+-$target\\.tar\\.gz\$" "$tmp/SHA256SUMS" | head -n 1 || true)
[ -n "$line" ] || die "the release has no build for $target"
sum=${line%% *}
file=${line##*[ *]}
version=${file#acs-}
version=${version%-"$target".tar.gz}
# Belt and braces: whatever the grep let through, this is a version.
case "$version" in
    *[!0-9.]* | *..* | .* | *. | "")
        die "the release names a version that is not X.Y.Z: $version"
        ;;
esac
if [ -n "$want" ] && [ "$version" != "$want" ]; then
    die "asked for $want, but the release holds $version"
fi

say "downloading acs $version for $target"
archive="$releases/download/$(release_tag "$version")/$file"
fetch "$archive" "$tmp/$file" || die "cannot download $archive"
got=$(sha256 "$tmp/$file")
if [ "$got" != "$sum" ]; then
    die "checksum mismatch for $file (expected $sum, got $got); nothing was installed"
fi
tar -xzf "$tmp/$file" -C "$tmp"
new="$tmp/acs-$version-$target/acs"
[ -f "$new" ] || die "$file does not hold acs-$version-$target/acs"
chmod 755 "$new"

# ---- where it goes -------------------------------------------------------------

# Under umask 002 (Ubuntu's default with user-private groups) what is made
# below would be group-writable, and acs on a remote refuses to run from a
# directory others can write (acs-91q2): installed by this script, it was
# refused and uploaded again on the first connection.
umask 022

# Copy to a temporary name next to the destination, then rename over it,
# so the path is never missing or half-written.
# An unguessable suffix for the temporary names below (acs-721). $$ is the
# process id, which anyone can guess: this script tells the user to re-run
# it as root into /usr/local, where a non-root user can often create files,
# and a name planted there ahead of us would be written through.
rand() {
    if [ -r /dev/urandom ]; then
        od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n'
    fi
}
tag=$(rand)
[ -n "$tag" ] || tag="$$.$(date +%s)"

place() { # place <source> <destination>
    # install creates the destination itself rather than writing through
    # whatever may be sitting at that name.
    rm -f "$2.new.$tag"
    if command -v install >/dev/null 2>&1; then
        install -m 755 "$1" "$2.new.$tag"
    else
        cp "$1" "$2.new.$tag"
        chmod 755 "$2.new.$tag"
    fi
    mv -f "$2.new.$tag" "$2"
}

link() { # link <target> <link>
    # ln -s fails rather than following a name already there.
    rm -f "$2.new.$tag"
    ln -s "$1" "$2.new.$tag"
    mv -f "$2.new.$tag" "$2"
}

# The physical path of directory $1, or a failure when it is not one.
phys() { (cd "$1" 2>/dev/null && pwd -P); }

# Whether $1, what $bin/acs points at, is $lib/<version>/<file>: by name,
# or through a symlinked parent ($HOME itself is often one).
ours() {
    case "$1" in "$lib"/*) return 0 ;; esac
    d=$(phys "$(dirname "$(dirname "$1")")") || return 1
    [ "$d" = "$(phys "$lib")" ]
}

# Whether version $1 (X.Y.Z) is at least $2, field by field as numbers, as
# release::compare orders them: a pre-release $2 is below its release, and
# a $2 that is not a version at all does not hold the link (should_link).
at_least() {
    case "$2" in [0-9]*.[0-9]*.[0-9]*) ;; *) return 0 ;; esac
    a=$1. b=${2%%-*}.
    while [ -n "$a" ] && [ -n "$b" ]; do
        x=${a%%.*} y=${b%%.*}
        case "$x$y" in *[!0-9]*) return 0 ;; esac
        [ "$x" -gt "$y" ] && return 0
        [ "$x" -lt "$y" ] && return 1
        a=${a#*.} b=${b#*.}
    done
    return 0
}

# Point $2 ($bin/acs) at $1 ($lib/$version/acs) by acs's own rule for that
# link (install::should_link, DESIGN §8), so this script -- run as
# `curl | sh`, often as root -- never replaces what it did not make: link
# when nothing is there, or when it is a link of ours to an older or
# removed version; keep a link of ours to a newer one; leave anything else
# alone (acs-feee). Sets $linked to linked, newer (with $have) or theirs.
link_bin() { # link_bin <target> <link>
    linked=theirs
    if [ -L "$2" ]; then
        cur=$(readlink "$2") || return 0
        case "$cur" in /*) ;; *) cur="$(dirname "$2")/$cur" ;; esac
        ours "$cur" || return 0
        have=$(basename "$(dirname "$cur")")
        if [ -e "$cur" ] && ! at_least "$version" "$have"; then
            linked=newer
            return 0
        fi
    elif [ -e "$2" ]; then
        return 0
    fi
    link "$1" "$2"
    linked=linked
}

# The second name of acs (layout::COPY_NAME; tests/scripts.rs holds the two
# together): run as acscp, it copies files.
copy_name=acscp

# Put $1/acscp -> acs beside an acs of ours (ACSCP_DESIGN §9), relative, so
# whatever repoints acs moves acscp with it. Only where $linked says the acs
# there is ours (linked, newer, or unset: placed by ACS_INSTALL_DIR); made
# where nothing is, kept when it is already a link to the bare name acs,
# and anything else the user put there is left alone, as $bin/acs is.
# Failing is a warning, never a failed install: `acs cp` needs no link.
# Sets $copied to linked, theirs, failed or skipped (the acs is theirs).
link_copy() { # link_copy <dir>
    copied=skipped
    case "${linked:-placed}" in linked | newer | placed) ;; *) return 0 ;; esac
    c="$1/$copy_name"
    if [ -L "$c" ]; then
        copied=theirs
        [ "$(readlink "$c")" = acs ] && copied=linked
        return 0
    elif [ -e "$c" ]; then
        copied=theirs
        return 0
    fi
    rm -f "$c.new.$tag"
    if ln -s acs "$c.new.$tag" 2>/dev/null && mv -f "$c.new.$tag" "$c" 2>/dev/null; then
        copied=linked
    else
        rm -f "$c.new.$tag"
        copied=failed
    fi
}

if [ -n "${ACS_INSTALL_DIR:-}" ]; then
    bin=$ACS_INSTALL_DIR
    mkdir -p "$bin" || die "cannot create $bin"
    [ -w "$bin" ] || die "cannot write to $bin (run as root, or choose another ACS_INSTALL_DIR)"
    place "$new" "$bin/acs"
    installed="$bin/acs"
else
    if [ "$(id -u)" = 0 ]; then
        lib=/usr/local/lib/acs
        bin=/usr/local/bin
    else
        [ -n "${HOME:-}" ] || die "HOME is not set"
        lib="$HOME/.local/share/acs"
        bin="$HOME/.local/bin"
    fi
    mkdir -p "$lib/$version" "$bin" || die "cannot create $lib/$version or $bin"
    # As the remote install does: a client refuses to run a binary whose
    # directory others can write, and would upload this version again.
    chmod 755 "$lib" "$lib/$version" || die "cannot set the mode of $lib/$version"
    place "$new" "$lib/$version/acs"
    installed="$lib/$version/acs"
    link_bin "$installed" "$bin/acs"
fi
link_copy "$bin"

case "${linked:-placed}" in
    placed) say "installed acs $version: $installed" ;;
    linked) say "installed acs $version: $bin/acs -> $installed" ;;
    newer)
        say "installed acs $version: $installed"
        say "left $bin/acs on acs $have, which is newer: an older version never takes the link back"
        ;;
    *)
        say "installed acs $version: $installed"
        say "left $bin/acs alone: it is not a link this installer made (remove it and run again to link it)"
        ;;
esac
case "$copied" in
    theirs) say "left $bin/$copy_name alone: it is not a link this installer made (\`acs cp\` works without it)" ;;
    failed) say "warning: cannot link $bin/$copy_name -> acs (\`acs cp\` works without it)" ;;
esac

# ---- after -------------------------------------------------------------------

case ":${PATH:-}:" in
    *":$bin:"*) ;;
    *)
        say "$bin is not on your PATH; add it with:"
        case "${SHELL:-}" in
            */fish) say "  fish_add_path $bin" ;;
            */zsh) say "  echo 'export PATH=\"$bin:\$PATH\"' >> ~/.zshrc" ;;
            */bash) say "  echo 'export PATH=\"$bin:\$PATH\"' >> ~/.bashrc" ;;
            *) say "  echo 'export PATH=\"$bin:\$PATH\"' >> ~/.profile" ;;
        esac
        ;;
esac

"$installed" --version
