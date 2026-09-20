#!/bin/sh
# Test scripts/install.sh against the real GitHub releases: here in a
# temporary HOME, then in Alpine (busybox wget, no curl) and Ubuntu
# containers, each as root and as a normal user.
#
#   scripts/test_install.sh           everything (needs docker for the containers)
#   scripts/test_install.sh --here    only in this environment
#
# Needs network access to github.com. ACS_TEST_OLD / ACS_TEST_NEW name two
# published versions (default 0.1.0 and the latest).
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
installer="$root/scripts/install.sh"
old=${ACS_TEST_OLD:-0.1.0}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}
pass() { echo "ok: $*"; }

# Run the installer with a clean environment plus the given assignments;
# output goes to $t/out, the exit status to $t/status.
run() {
    set +e
    env -i PATH="$fakebin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        HOME="$t/home" SHELL=/bin/zsh TMPDIR="$t" "$@" sh "$installer" >"$t/out" 2>&1
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

    # A download that does not match SHA256SUMS installs nothing.
    mirror="$t/mirror/download/v$new"
    mkdir -p "$mirror"
    echo "$latest" >"$mirror/SHA256SUMS"
    for f in $(echo "$latest" | sed 's/.* //'); do
        echo "not the real archive" >"$mirror/$f"
    done
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
    run ACS_RELEASES_URL="file://$t/mirror" ACS_ALLOW_INSECURE_URL=1 ACS_VERSION="$new" ACS_INSTALL_DIR="$t/tampered"
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
        alpine*) setup='adduser -D tester' ;;
        # Ubuntu's image has neither curl nor wget: install one, as a user would.
        ubuntu*) setup='apt-get -qq update >/dev/null && apt-get -qq install -y curl ca-certificates >/dev/null && useradd -m tester' ;;
    esac
    docker run --rm -v "$root:/src:ro" "$image" sh -c "
        set -e
        $setup
        sh /src/scripts/test_install.sh --here
        su tester -c 'sh /src/scripts/test_install.sh --here'
    " || fail "$image"
done
echo "== all install.sh tests passed"
