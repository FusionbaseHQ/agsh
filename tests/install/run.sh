#!/usr/bin/env sh
# Offline regression tests for install.sh. curl and uname are replaced with
# deterministic fixtures; no user files, network, or published releases are used.
set -eu
umask 077

ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)

case $# in
0)
    /bin/sh "$ROOT/tests/install/run.sh" linux-x86_64
    /bin/sh "$ROOT/tests/install/run.sh" darwin-arm64
    printf '%s\n' 'installer tests: ok (all platform fixtures)'
    exit 0
    ;;
1) FIXTURE_PLATFORM=$1 ;;
*)
    printf '%s\n' "usage: $0 [linux-x86_64|darwin-arm64]" >&2
    exit 64
    ;;
esac

case "$FIXTURE_PLATFORM" in
linux-x86_64)
    FIXTURE_UNAME_OS=Linux
    FIXTURE_UNAME_ARCH=x86_64
    FIXTURE_TARGET=x86_64-unknown-linux-musl
    FIXTURE_INTERCEPT_LIB=libagsh_intercept.so
    FIXTURE_PLATFORM_COPYRIGHT=MUSL_COPYRIGHT.txt
    ;;
darwin-arm64)
    FIXTURE_UNAME_OS=Darwin
    FIXTURE_UNAME_ARCH=arm64
    FIXTURE_TARGET=aarch64-apple-darwin
    FIXTURE_INTERCEPT_LIB=libagsh_intercept.dylib
    FIXTURE_PLATFORM_COPYRIGHT=
    ;;
*)
    printf '%s\n' "unknown installer fixture: $FIXTURE_PLATFORM" >&2
    exit 64
    ;;
esac

TMP=$(mktemp -d "${TMPDIR:-/tmp}/agsh-install-test.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM
mkdir -p "$TMP/fake-bin" "$TMP/home" "$TMP/package"

cat >"$TMP/fake-bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
-s) printf '%s\n' "$FIXTURE_UNAME_OS" ;;
-m) printf '%s\n' "$FIXTURE_UNAME_ARCH" ;;
*) printf '%s\n' "$FIXTURE_UNAME_OS" ;;
esac
EOF

cat >"$TMP/fake-bin/curl" <<'EOF'
#!/bin/sh
out=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
    -o)
        out=$2
        shift 2
        ;;
    https://*)
        url=$1
        shift
        ;;
    *) shift ;;
    esac
done
[ -n "$out" ] && [ -n "$url" ] || exit 64
printf '%s\n' "$url" >>"$CURL_URL_LOG"
case "$url" in
*/checksums.txt) cp "$FIXTURE_CHECKSUMS" "$out" ;;
*.tar.gz) cp "$FIXTURE_ARCHIVE" "$out" ;;
*) exit 65 ;;
esac
EOF

cat >"$TMP/fake-bin/gh" <<'EOF'
#!/bin/sh
set -eu
if [ "${1:-}" = --version ]; then
    printf 'gh version %s (fixture)\n' "${FAKE_GH_VERSION:-2.97.0}"
    exit 0
fi
[ "${1:-}" = release ] || exit 64
shift
[ "${1:-}" = verify-asset ] || exit 64
shift
tag=${1:-}
artifact=${2:-}
[ -n "$tag" ] && [ -n "$artifact" ] || exit 64
shift 2

repo=
while [ "$#" -gt 0 ]; do
    case "$1" in
    -R)
        repo=$2
        shift 2
        ;;
    *) exit 64 ;;
    esac
done

[ "$tag" = v0.2.0 ] || exit 65
[ "${artifact##*/}" = "$EXPECTED_ARCHIVE_NAME" ] || exit 65
[ "$repo" = FusionbaseHQ/agsh ] || exit 65
printf '%s\n' "$tag" >"$GH_ARGS_LOG"
printf '%s\n' "${artifact##*/}" >"$GH_ASSET_LOG"
[ "${FAKE_GH_RESULT:-pass}" = pass ]
EOF
chmod 755 "$TMP/fake-bin/uname" "$TMP/fake-bin/curl" "$TMP/fake-bin/gh"

NAME="agsh-v0.2.0-$FIXTURE_TARGET"
PAYLOAD="$TMP/package/$NAME"
mkdir -p "$PAYLOAD/LICENSES"
cat >"$PAYLOAD/agsh" <<'EOF'
#!/bin/sh
printf '%s\n' 'agsh 0.2.0'
EOF
cat >"$PAYLOAD/agsh-exec-helper" <<'EOF'
#!/bin/sh
case "${1:-}" in
--version) printf '%s\n' 'agsh-exec-helper 0.2.0' ;;
*) exit 2 ;;
esac
EOF
chmod 755 "$PAYLOAD/agsh" "$PAYLOAD/agsh-exec-helper"
printf 'fixture library\n' >"$PAYLOAD/$FIXTURE_INTERCEPT_LIB"
for file in LICENSE NOTICE THIRD_PARTY_LICENSES.html \
    RUST_STANDARD_LIBRARY_COPYRIGHT.html README.md CHANGELOG.md
do
    printf 'fixture %s\n' "$file" >"$PAYLOAD/$file"
done
[ -z "$FIXTURE_PLATFORM_COPYRIGHT" ] ||
    printf 'fixture %s\n' "$FIXTURE_PLATFORM_COPYRIGHT" >"$PAYLOAD/$FIXTURE_PLATFORM_COPYRIGHT"
printf 'fixture Apache license\n' >"$PAYLOAD/LICENSES/Apache-2.0.txt"

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

make_fixture() {
    archive=$1
    checksums=$2
    tar -C "$TMP/package" -czf "$archive" "$NAME"
    printf '%s  %s.tar.gz\n' "$(checksum "$archive")" "$NAME" >"$checksums"
}

run_installer() {
    install_dir=$1
    PATH="$TMP/fake-bin:/usr/bin:/bin" \
        HOME="$TMP/home" \
        AGSH_VERSION=v0.2.0 \
        AGSH_INSTALL_DIR="$install_dir" \
        AGSH_DOC_DIR="$install_dir/docs" \
        AGSH_REQUIRE_ATTESTATION="${TEST_REQUIRE_ATTESTATION:-1}" \
        FAKE_GH_RESULT="${TEST_GH_RESULT:-pass}" \
        FAKE_GH_VERSION="${TEST_GH_VERSION:-2.97.0}" \
        GH_ARGS_LOG="$TMP/gh.args" \
        GH_ASSET_LOG="$TMP/gh.asset" \
        CURL_URL_LOG="$TMP/curl.urls" \
        EXPECTED_ARCHIVE_NAME="$NAME.tar.gz" \
        FIXTURE_UNAME_OS="$FIXTURE_UNAME_OS" \
        FIXTURE_UNAME_ARCH="$FIXTURE_UNAME_ARCH" \
        FIXTURE_ARCHIVE="$FIXTURE_ARCHIVE" \
        FIXTURE_CHECKSUMS="$FIXTURE_CHECKSUMS" \
        /bin/sh "$ROOT/install.sh"
}

FIXTURE_ARCHIVE="$TMP/release.tar.gz"
FIXTURE_CHECKSUMS="$TMP/checksums.txt"
export FIXTURE_ARCHIVE FIXTURE_CHECKSUMS
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"
mkdir -p "$TMP/install"
printf 'do not overwrite through symlink\n' >"$TMP/victim"
ln -s "$TMP/victim" "$TMP/install/agsh"
run_installer "$TMP/install" >/dev/null 2>"$TMP/install.err"
test -x "$TMP/install/agsh"
test ! -L "$TMP/install/agsh"
test "$(cat "$TMP/victim")" = "do not overwrite through symlink"
test -x "$TMP/install/agsh-exec-helper"
test "$("$TMP/install/agsh-exec-helper" --version)" = "agsh-exec-helper 0.2.0"
test -x "$TMP/install/$FIXTURE_INTERCEPT_LIB"
test "$("$TMP/install/agsh" --version)" = "agsh 0.2.0"
test -f "$TMP/install/docs/LICENSE"
test -f "$TMP/install/docs/NOTICE"
test -f "$TMP/install/docs/THIRD_PARTY_LICENSES.html"
test -f "$TMP/install/docs/RUST_STANDARD_LIBRARY_COPYRIGHT.html"
test -f "$TMP/install/docs/Apache-2.0.txt"
test "$(cat "$TMP/gh.args")" = v0.2.0
test "$(cat "$TMP/gh.asset")" = "$NAME.tar.gz"
grep -F -x -q \
    "https://github.com/FusionbaseHQ/agsh/releases/download/v0.2.0/$NAME.tar.gz" \
    "$TMP/curl.urls"
case "$FIXTURE_PLATFORM" in
linux-x86_64)
    test "$NAME.tar.gz" = agsh-v0.2.0-x86_64-unknown-linux-musl.tar.gz
    test -f "$TMP/install/docs/MUSL_COPYRIGHT.txt"
    test ! -e "$TMP/install/libagsh_intercept.dylib"
    ;;
darwin-arm64)
    test "$NAME.tar.gz" = agsh-v0.2.0-aarch64-apple-darwin.tar.gz
    test ! -e "$TMP/install/docs/MUSL_COPYRIGHT.txt"
    test -x "$TMP/install/libagsh_intercept.dylib"
    test ! -e "$TMP/install/libagsh_intercept.so"
    ;;
esac

# Required immutable-release verification must fail closed before extraction.
# The fake gh command also rejects a missing or inexact release tag above.
if TEST_GH_RESULT=fail run_installer "$TMP/attestation-fail" \
    >/dev/null 2>"$TMP/attestation-fail.err"
then
    printf '%s\n' 'failed required attestation was accepted' >&2
    exit 1
fi
grep -q 'attestation verification failed' "$TMP/attestation-fail.err"
test ! -e "$TMP/attestation-fail/agsh"
unset TEST_GH_RESULT

# Vulnerable GitHub CLI versions must never receive credentials during a
# release-attestation request. Required mode fails before invoking verify-asset.
rm -f "$TMP/gh.args" "$TMP/gh.asset"
if TEST_GH_VERSION=2.96.0 run_installer "$TMP/old-gh" \
    >/dev/null 2>"$TMP/old-gh.err"
then
    printf '%s\n' 'unsafe GitHub CLI version was accepted for attestation' >&2
    exit 1
fi
grep -F -q 'GitHub CLI 2.97.0 or newer' "$TMP/old-gh.err"
test ! -e "$TMP/gh.args"
test ! -e "$TMP/gh.asset"
test ! -e "$TMP/old-gh/agsh"
unset TEST_GH_VERSION

# A destination symlink resolving to a directory must not receive a staged
# file or be silently accepted as the installed executable.
mkdir -p "$TMP/directory-target" "$TMP/directory-link-install"
ln -s "$TMP/directory-target" "$TMP/directory-link-install/agsh"
if run_installer "$TMP/directory-link-install" >/dev/null 2>"$TMP/directory-link.err"; then
    printf '%s\n' 'directory destination symlink was accepted' >&2
    exit 1
fi
grep -q 'refusing to replace directory destination' "$TMP/directory-link.err"
test -z "$(find "$TMP/directory-target" -mindepth 1 -maxdepth 1 -print -quit)"

# A validly checksummed archive for the wrong binary version must fail before
# creating the requested install directory.
sed 's/agsh 0.2.0/agsh 9.9.9/' "$PAYLOAD/agsh" >"$PAYLOAD/agsh.wrong"
mv "$PAYLOAD/agsh.wrong" "$PAYLOAD/agsh"
chmod 755 "$PAYLOAD/agsh"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"
if run_installer "$TMP/wrong-version" >/dev/null 2>"$TMP/wrong-version.err"; then
    printf '%s\n' 'wrong-version binary was accepted' >&2
    exit 1
fi
grep -q 'binary version does not match' "$TMP/wrong-version.err"
test ! -e "$TMP/wrong-version/agsh"
test ! -e "$TMP/wrong-version/agsh-exec-helper"
test ! -e "$TMP/wrong-version/$FIXTURE_INTERCEPT_LIB"
sed 's/agsh 9.9.9/agsh 0.2.0/' "$PAYLOAD/agsh" >"$PAYLOAD/agsh.correct"
mv "$PAYLOAD/agsh.correct" "$PAYLOAD/agsh"
chmod 755 "$PAYLOAD/agsh"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"

# The launch helper is a version-coupled executable and must be validated
# before either executable becomes visible at its final path.
sed 's/agsh-exec-helper 0.2.0/agsh-exec-helper 9.9.9/' \
    "$PAYLOAD/agsh-exec-helper" >"$PAYLOAD/agsh-exec-helper.wrong"
mv "$PAYLOAD/agsh-exec-helper.wrong" "$PAYLOAD/agsh-exec-helper"
chmod 755 "$PAYLOAD/agsh-exec-helper"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"
if run_installer "$TMP/wrong-helper-version" \
    >/dev/null 2>"$TMP/wrong-helper-version.err"
then
    printf '%s\n' 'wrong-version exec helper was accepted' >&2
    exit 1
fi
grep -q 'exec helper version does not match' "$TMP/wrong-helper-version.err"
test ! -e "$TMP/wrong-helper-version/agsh"
test ! -e "$TMP/wrong-helper-version/agsh-exec-helper"
sed 's/agsh-exec-helper 9.9.9/agsh-exec-helper 0.2.0/' \
    "$PAYLOAD/agsh-exec-helper" >"$PAYLOAD/agsh-exec-helper.correct"
mv "$PAYLOAD/agsh-exec-helper.correct" "$PAYLOAD/agsh-exec-helper"
chmod 755 "$PAYLOAD/agsh-exec-helper"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"

# A checksum manifest must identify the asset exactly once.
cat "$TMP/checksums.txt" >>"$TMP/checksums.txt.duplicate"
cat "$TMP/checksums.txt" >>"$TMP/checksums.txt.duplicate"
FIXTURE_CHECKSUMS="$TMP/checksums.txt.duplicate"
export FIXTURE_CHECKSUMS
if run_installer "$TMP/duplicate" >/dev/null 2>"$TMP/duplicate.err"; then
    printf '%s\n' 'duplicate checksum was accepted' >&2
    exit 1
fi
grep -q 'duplicate entries' "$TMP/duplicate.err"

# A well-formed but incorrect digest must fail before extraction.
printf '%064d  %s.tar.gz\n' 0 "$NAME" >"$TMP/checksums.bad"
FIXTURE_CHECKSUMS="$TMP/checksums.bad"
export FIXTURE_CHECKSUMS
if run_installer "$TMP/bad-checksum" >/dev/null 2>"$TMP/bad-checksum.err"; then
    printf '%s\n' 'incorrect checksum was accepted' >&2
    exit 1
fi
grep -q 'checksum mismatch' "$TMP/bad-checksum.err"

# Even a checksum-matching archive cannot smuggle additional members.
FIXTURE_CHECKSUMS="$TMP/checksums.txt"
printf 'unexpected\n' >"$PAYLOAD/unexpected"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"
export FIXTURE_CHECKSUMS
if run_installer "$TMP/unexpected" >/dev/null 2>"$TMP/unexpected.err"; then
    printf '%s\n' 'unexpected archive member was accepted' >&2
    exit 1
fi
grep -q 'too many members' "$TMP/unexpected.err"
rm "$PAYLOAD/unexpected"

# Expected names must still be regular files, not symlinks.
rm "$PAYLOAD/agsh"
ln -s /bin/sh "$PAYLOAD/agsh"
make_fixture "$FIXTURE_ARCHIVE" "$FIXTURE_CHECKSUMS"
if run_installer "$TMP/symlink" >/dev/null 2>"$TMP/symlink.err"; then
    printf '%s\n' 'symlink payload was accepted' >&2
    exit 1
fi
grep -q 'regular agsh binary' "$TMP/symlink.err"

printf 'installer tests (%s): ok\n' "$FIXTURE_PLATFORM"
