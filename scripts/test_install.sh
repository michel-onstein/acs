#!/bin/sh
# Test scripts/install.sh against the real GitHub releases: here in a
# temporary HOME, then in Alpine (busybox wget, no curl) and Ubuntu
# containers, each as root and as a normal user.
#
#   scripts/test_install.sh           everything (needs docker for the containers)
#   scripts/test_install.sh --here    only in this environment
#
# Needs network access to github.com. ACS_TEST_OLD / ACS_TEST_NEW name two
# published versions (default 0.12.0, the first signed release, and the
# latest); both must carry a SHA256SUMS.sig, which the installer requires.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
installer="$root/scripts/install.sh"
old=${ACS_TEST_OLD:-0.12.0}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}
pass() { echo "ok: $*"; }

# Run the installer ($script, the repository's unless a case says otherwise)
# with a clean environment plus the given assignments; output goes to
# $t/out, the exit status to $t/status.
run() {
    set +e
    env -i PATH="$fakebin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        HOME="$t/home" SHELL=/bin/zsh TMPDIR="$t" "$@" sh "$script" >"$t/out" 2>&1
    echo $? >"$t/status"
    set -e
}

status() { cat "$t/status"; }
out() { cat "$t/out"; }
expect_ok() { [ "$(status)" = 0 ] || fail "$1: exit $(status): $(out)"; }
expect_fail() { [ "$(status)" != 0 ] || fail "$1: should fail: $(out)"; }
expect_out() { grep -qF -- "$2" "$t/out" || fail "$1: no '$2' in: $(out)"; }

here() {
    t=$(mktemp -d "${TMPDIR:-/tmp}/acs-install-test.XXXXXX")
    trap 'rm -rf "$t"' EXIT
    fakebin="$t/fakebin"
    script=$installer
    mkdir -p "$fakebin" "$t/home"
    who=$(id -un 2>/dev/null || id -u)
    latest=$(curl -fsSL https://github.com/michel-onstein/acs/releases/latest/download/SHA256SUMS 2>/dev/null ||
        wget -q -O - https://github.com/michel-onstein/acs/releases/latest/download/SHA256SUMS)
    new=${ACS_TEST_NEW:-$(echo "$latest" | sed -n 's/.*acs-\([0-9][^-]*\)-.*/\1/p' | head -n 1)}
    [ -n "$new" ] || fail "cannot tell the latest release"
    echo "== install.sh as $who on $(uname -s) $(uname -m): $old and $new"

    if [ "$(id -u)" = 0 ]; then
        lib=/usr/local/lib/acs
        bin=/usr/local/bin
    else
        lib="$t/home/.local/share/acs"
        bin="$t/home/.local/bin"
    fi

    # The latest release, into the default place.
    run
    expect_ok "default"
    expect_out "default" "acs $new ("
    [ -x "$lib/$new/acs" ] || fail "default: no $lib/$new/acs"
    [ "$(readlink "$bin/acs")" = "$lib/$new/acs" ] || fail "default: $bin/acs -> $(readlink "$bin/acs")"
    if [ "$(id -u)" != 0 ]; then
        expect_out "default" "$bin is not on your PATH"
        expect_out "default" ">> ~/.zshrc"
    fi
    pass "latest ($new) into $bin, linked to $lib/$new"

    # A pinned older version, then back to the latest: upgrades in place.
    run ACS_VERSION="$old"
    expect_ok "pinned"
    expect_out "pinned" "acs $old ("
    [ "$(readlink "$bin/acs")" = "$lib/$old/acs" ] || fail "pinned: $bin/acs -> $(readlink "$bin/acs")"
    run ACS_VERSION="v$new"
    expect_ok "re-run"
    [ "$(readlink "$bin/acs")" = "$lib/$new/acs" ] || fail "re-run: not upgraded"
    case $("$bin/acs" --version) in "acs $new ("*) ;; *) fail "re-run: $bin/acs is not $new" ;; esac
    pass "pinned $old, then upgraded in place to $new"

    # A directory of one's own: the binary itself goes there.
    run ACS_INSTALL_DIR="$t/own bin"
    expect_ok "ACS_INSTALL_DIR"
    [ -f "$t/own bin/acs" ] && [ ! -L "$t/own bin/acs" ] || fail "ACS_INSTALL_DIR: no plain file"
    case $("$t/own bin/acs" --version) in "acs $new ("*) ;; *) fail "ACS_INSTALL_DIR: wrong binary" ;; esac
    pass "ACS_INSTALL_DIR"

    # A local mirror: the real SHA256SUMS, signed by a throwaway key, and
    # archives that do not match it. The installer only reads SHA256SUMS
    # once its signature checks out, so reaching the checksum check takes
    # a copy of the installer that trusts the throwaway key -- substituted
    # the way `cargo xtask package` substitutes a fork's (acs-x57).
    mirror="$t/mirror/download/v$new"
    mkdir -p "$mirror"
    echo "$latest" >"$mirror/SHA256SUMS"
    for f in $(echo "$latest" | sed 's/.* //'); do
        echo "not the real archive" >"$mirror/$f"
    done
    ssh-keygen -q -t ed25519 -N '' -C 'acs test signing' -f "$t/testkey"
    ssh-keygen -q -Y sign -f "$t/testkey" -n acs-release "$mirror/SHA256SUMS"
    [ -s "$mirror/SHA256SUMS.sig" ] || fail "cannot sign the mirror"
    testkey=$(cat "$t/testkey.pub")
    sed "s|^release_key=.*|release_key='$testkey'|" "$installer" >"$t/install-testkey.sh"
    grep -qxF "release_key='$testkey'" "$t/install-testkey.sh" || fail "cannot substitute the test key"
    if ! command -v curl >/dev/null; then
        # busybox wget has no file:// URLs: a shim serves the mirror.
        real=$(command -v wget)
        cat >"$fakebin/wget" <<EOF
#!/bin/sh
eval "url=\\\${\$#}"
case "\$url" in
    file://*) cp "\${url#file://}" "\$3" ;;
    *) exec $real "\$@" ;;
esac
EOF
        chmod 755 "$fakebin/wget"
    fi

    # Signed, but not by the acs release key: the real installer refuses
    # before it reads a line of SHA256SUMS.
    run ACS_RELEASES_URL="file://$t/mirror" ACS_ALLOW_INSECURE_URL=1 ACS_VERSION="$new" ACS_INSTALL_DIR="$t/tampered"
    expect_fail "foreign signature"
    expect_out "foreign signature" "is not signed by the acs release key; nothing was installed"
    [ ! -e "$t/tampered/acs" ] || fail "foreign signature: something was installed"
    pass "SHA256SUMS signed by another key refused"

    # Signed by the key it trusts, so the archive itself is what is wrong.
    script="$t/install-testkey.sh"
    run ACS_RELEASES_URL="file://$t/mirror" ACS_ALLOW_INSECURE_URL=1 ACS_VERSION="$new" ACS_INSTALL_DIR="$t/tampered"
    script=$installer
    rm -f "$fakebin/wget"
    expect_fail "checksum"
    expect_out "checksum" "checksum mismatch for acs-$new-"
    expect_out "checksum" "nothing was installed"
    [ ! -e "$t/tampered/acs" ] || fail "checksum: something was installed"
    pass "checksum mismatch refused"

    # A version that does not exist.
    run ACS_VERSION=0.0.0 ACS_INSTALL_DIR="$t/none"
    expect_fail "no such version"
    expect_out "no such version" "is 0.0.0 a release?"
    pass "missing version refused"

    # A platform without a build.
    # shellcheck disable=SC2016 # $1 is the fake uname's own argument
    printf '#!/bin/sh\ncase "$1" in -m) echo sparc64;; *) echo SunOS;; esac\n' >"$fakebin/uname"
    chmod 755 "$fakebin/uname"
    run ACS_INSTALL_DIR="$t/none"
    expect_fail "unsupported"
    expect_out "unsupported" "acs has no build for SunOS sparc64"
    rm "$fakebin/uname"
    pass "unsupported platform refused"
}

if [ "${1:-}" = --here ]; then
    here
    exit 0
fi

command -v shellcheck >/dev/null && shellcheck "$installer" && pass "shellcheck"
here

command -v docker >/dev/null || fail "docker is needed for the Linux containers (or use --here)"
for image in alpine:3 ubuntu:24.04; do
    case $image in
        # Neither image has ssh-keygen, which the installer needs to check
        # the release signature; a host that runs acs has it with ssh.
        alpine*) setup='apk add -q --no-cache openssh-keygen && adduser -D tester' ;;
        # Ubuntu's image has neither curl nor wget either: install one, as a user would.
        ubuntu*) setup='apt-get -qq update >/dev/null && apt-get -qq install -y curl ca-certificates openssh-client >/dev/null && useradd -m tester' ;;
    esac
    docker run --rm -v "$root:/src:ro" "$image" sh -c "
        set -e
        $setup
        sh /src/scripts/test_install.sh --here
        su tester -c 'sh /src/scripts/test_install.sh --here'
    " || fail "$image"
done
echo "== all install.sh tests passed"
