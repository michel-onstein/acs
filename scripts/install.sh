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
# Running it again upgrades in place.
#
# Environment:
#   ACS_VERSION=X.Y.Z     install that release instead of the latest
#   ACS_INSTALL_DIR=DIR   put the binary itself in DIR (no versioned layout)
#   ACS_RELEASES_URL=URL  where releases are (default: the GitHub releases)
set -eu

releases=${ACS_RELEASES_URL:-https://github.com/michel-onstein/acs/releases}
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

tmp=$(mktemp -d "${TMPDIR:-/tmp}/acs-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# SHA256SUMS names every archive, so it also tells the latest version.
if [ -n "$want" ]; then
    base="$releases/download/v$want"
else
    base="$releases/latest/download"
fi
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "cannot download $base/SHA256SUMS${want:+ (is $want a release?)}"
line=$(grep -E "^[0-9a-f]{64} [ *]?acs-[^ ]+-$target\\.tar\\.gz\$" "$tmp/SHA256SUMS" | head -n 1 || true)
[ -n "$line" ] || die "the release has no build for $target"
sum=${line%% *}
file=${line##* }
file=${file#\*}
version=${file#acs-}
version=${version%-"$target".tar.gz}
if [ -n "$want" ] && [ "$version" != "$want" ]; then
    die "asked for $want, but the release holds $version"
fi

say "downloading acs $version for $target"
fetch "$releases/download/v$version/$file" "$tmp/$file" ||
    die "cannot download $releases/download/v$version/$file"
got=$(sha256 "$tmp/$file")
if [ "$got" != "$sum" ]; then
    die "checksum mismatch for $file (expected $sum, got $got); nothing was installed"
fi
tar -xzf "$tmp/$file" -C "$tmp"
new="$tmp/acs-$version-$target/acs"
[ -f "$new" ] || die "$file does not hold acs-$version-$target/acs"
chmod 755 "$new"

# ---- where it goes -------------------------------------------------------------

# Copy to a temporary name next to the destination, then rename over it,
# so the path is never missing or half-written.
place() { # place <source> <destination>
    cp "$1" "$2.new.$$"
    chmod 755 "$2.new.$$"
    mv -f "$2.new.$$" "$2"
}

link() { # link <target> <link>
    ln -s "$1" "$2.new.$$"
    mv -f "$2.new.$$" "$2"
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
    place "$new" "$lib/$version/acs"
    link "$lib/$version/acs" "$bin/acs"
    installed="$lib/$version/acs"
fi

say "installed acs $version: $bin/acs"

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
