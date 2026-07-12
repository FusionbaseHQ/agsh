#!/bin/sh
# agsh installer — https://github.com/FusionbaseHQ/agsh
#
#   curl --proto '=https' --tlsv1.2 -fsSLo install.sh \
#     https://github.com/FusionbaseHQ/agsh/releases/latest/download/install.sh
#   less install.sh && sh install.sh
#
# Downloads the prebuilt binary for this platform from GitHub Releases,
# verifies its SHA-256 against the release's checksums.txt, and installs it.
# No sudo needed. Environment overrides:
#
#   AGSH_VERSION      release tag to install (default: latest, e.g. v0.3.0)
#   AGSH_INSTALL_DIR  install directory (default: ~/.local/bin)
#   AGSH_DOC_DIR      license/notice directory (default: <install>/../share/doc/agsh)
#   AGSH_REQUIRE_ATTESTATION=1  require `gh attestation verify` to pass
#
# The commands above are the recommended installation path.

set -eu
umask 077

REPO="FusionbaseHQ/agsh"
INSTALL_DIR="${AGSH_INSTALL_DIR:-$HOME/.local/bin}"
DOC_DIR="${AGSH_DOC_DIR:-$INSTALL_DIR/../share/doc/agsh}"
VERSION="${AGSH_VERSION:-latest}"

say() { printf '%s\n' "$*" >&2; }
fail() {
    say "agsh-install: error: $*"
    exit 1
}

for tool in awk curl grep install mktemp mv rm tar uname; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

download() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
        --fail --silent --show-error --location --retry 3 --connect-timeout 15 \
        --max-time 300 --max-filesize 134217728 "$@"
}

# ---- platform detection -> Rust target triple -------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
Darwin)
    intercept_lib="libagsh_intercept.dylib"
    platform_copyright=
    case "$arch" in
    arm64 | aarch64) target="aarch64-apple-darwin" ;;
    x86_64) target="x86_64-apple-darwin" ;;
    *) fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
Linux)
    intercept_lib="libagsh_intercept.so"
    platform_copyright="MUSL_COPYRIGHT.txt"
    case "$arch" in
    x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
    aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
    *) fail "unsupported Linux architecture: $arch" ;;
    esac
    ;;
*) fail "unsupported OS: $os (agsh supports macOS and Linux)" ;;
esac

# ---- resolve "latest" to a concrete tag (via redirect; no API, no jq) --------
if [ "$VERSION" = "latest" ]; then
    effective=$(download --head -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest") ||
        fail "cannot reach github.com to resolve the latest release"
    VERSION=${effective##*/}
    case "$VERSION" in
    v*) ;;
    *) fail "could not resolve the latest release tag (got: $VERSION)" ;;
    esac
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
printf '%s\n' "$VERSION" |
    grep -E -q '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$' ||
    fail "invalid release version: $VERSION"

name="agsh-$VERSION-$target"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp=$(mktemp -d)
stage=
binary_stage=
cleanup() {
    [ -z "$stage" ] || rm -f "$stage"
    [ -z "$binary_stage" ] || rm -f "$binary_stage"
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' INT TERM

# ---- download + verify -------------------------------------------------------
say "Downloading agsh $VERSION ($target)…"
download "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" ||
    fail "download failed: $base/$name.tar.gz"
download --max-filesize 1048576 "$base/checksums.txt" -o "$tmp/checksums.txt" ||
    fail "could not download checksums.txt for verification"

expected=$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$tmp/checksums.txt")
[ -n "$expected" ] || fail "no checksum entry for $name.tar.gz in checksums.txt"
matches=$(printf '%s\n' "$expected" | awk 'NF { n++ } END { print n + 0 }')
[ "$matches" -eq 1 ] || fail "checksums.txt has duplicate entries for $name.tar.gz"
case "$expected" in *[!0-9a-f]*) fail "invalid SHA-256 in checksums.txt" ;; esac
[ "${#expected}" -eq 64 ] || fail "invalid SHA-256 length in checksums.txt"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$name.tar.gz" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{print $1}')
else
    fail "sha256sum or shasum is required"
fi
[ "$expected" = "$actual" ] ||
    fail "checksum mismatch for $name.tar.gz
  expected: $expected
  actual:   $actual
The download may be corrupted or tampered with — not installing."

if command -v gh >/dev/null 2>&1; then
    if gh attestation verify "$tmp/$name.tar.gz" -R "$REPO" \
        --signer-workflow "$REPO/.github/workflows/release.yml" \
        --source-ref "refs/tags/$VERSION" \
        --deny-self-hosted-runners >/dev/null 2>&1; then
        say "Verified GitHub artifact attestation."
    elif [ "${AGSH_REQUIRE_ATTESTATION:-}" = "1" ]; then
        fail "GitHub artifact attestation verification failed (ensure gh supports --source-ref)"
    else
        say "NOTE: GitHub artifact attestation was unavailable or failed; checksum matched."
    fi
elif [ "${AGSH_REQUIRE_ATTESTATION:-}" = "1" ]; then
    fail "AGSH_REQUIRE_ATTESTATION=1 requires GitHub CLI (`gh`)"
else
    say "NOTE: checksum verification detects corruption but is not independent"
    say "      release authentication. Install GitHub CLI and set"
    say "      AGSH_REQUIRE_ATTESTATION=1 for provenance verification."
fi

# ---- install -----------------------------------------------------------------
archive_list="$tmp/archive.list"
archive_member_count=11
[ -z "$platform_copyright" ] || archive_member_count=$((archive_member_count + 1))
if ! tar -tzf "$tmp/$name.tar.gz" |
    awk -v cap="$archive_member_count" 'NR > cap { exit 1 } { print }' >"$archive_list"
then
    fail "archive is unreadable or contains too many members"
fi
for member in \
    "$name/" \
    "$name/agsh" \
    "$name/$intercept_lib" \
    "$name/LICENSE" \
    "$name/NOTICE" \
    "$name/THIRD_PARTY_LICENSES.html" \
    "$name/RUST_STANDARD_LIBRARY_COPYRIGHT.html" \
    "$name/README.md" \
    "$name/CHANGELOG.md" \
    "$name/LICENSES/" \
    "$name/LICENSES/Apache-2.0.txt"
do
    [ "$(grep -F -x -c "$member" "$archive_list")" -eq 1 ] ||
        fail "archive is missing or duplicates expected member: $member"
done
[ -z "$platform_copyright" ] ||
    [ "$(grep -F -x -c "$name/$platform_copyright" "$archive_list")" -eq 1 ] ||
    fail "archive is missing or duplicates expected member: $name/$platform_copyright"
while IFS= read -r member; do
    case "$member" in
    "$name/" | "$name/agsh" | "$name/$intercept_lib" | \
        "$name/LICENSE" | "$name/NOTICE" | "$name/THIRD_PARTY_LICENSES.html" | \
        "$name/RUST_STANDARD_LIBRARY_COPYRIGHT.html" | "$name/README.md" | \
        "$name/CHANGELOG.md" | "$name/LICENSES/" | \
        "$name/LICENSES/Apache-2.0.txt") ;;
    *)
        if [ -z "$platform_copyright" ] ||
            [ "$member" != "$name/$platform_copyright" ]
        then
            fail "archive contains unexpected member: $member"
        fi
        ;;
    esac
done <"$archive_list"

extract="$tmp/extract"
mkdir -p "$extract/$name"
set -- \
    "$name/agsh" "$name/$intercept_lib" "$name/LICENSE" "$name/NOTICE" \
    "$name/THIRD_PARTY_LICENSES.html" "$name/RUST_STANDARD_LIBRARY_COPYRIGHT.html" \
    "$name/LICENSES/Apache-2.0.txt"
[ -z "$platform_copyright" ] || set -- "$@" "$name/$platform_copyright"
(ulimit -f 131072 2>/dev/null || true
 tar -xzf "$tmp/$name.tar.gz" -C "$extract" "$@") ||
    fail "could not extract release payload"
[ -f "$extract/$name/agsh" ] && [ ! -L "$extract/$name/agsh" ] ||
    fail "archive did not contain a regular agsh binary"
[ -f "$extract/$name/$intercept_lib" ] && [ ! -L "$extract/$name/$intercept_lib" ] ||
    fail "archive did not contain a regular interception library"
for member in LICENSE NOTICE THIRD_PARTY_LICENSES.html \
    RUST_STANDARD_LIBRARY_COPYRIGHT.html LICENSES/Apache-2.0.txt
do
    [ -f "$extract/$name/$member" ] && [ ! -L "$extract/$name/$member" ] ||
        fail "archive did not contain a regular $member"
done
if [ -n "$platform_copyright" ]; then
    [ -f "$extract/$name/$platform_copyright" ] &&
        [ ! -L "$extract/$name/$platform_copyright" ] ||
        fail "archive did not contain a regular $platform_copyright"
fi
mkdir -p "$INSTALL_DIR"
mkdir -p "$DOC_DIR"
install_atomic() {
    source_file=$1
    destination=$2
    mode=$3
    destination_dir=${destination%/*}
    destination_name=${destination##*/}
    [ ! -d "$destination" ] ||
        fail "refusing to replace directory destination: $destination"
    stage=$(mktemp "$destination_dir/.$destination_name.XXXXXX") ||
        fail "could not create install staging file in $destination_dir"
    install -m "$mode" "$source_file" "$stage" || fail "could not stage $destination_name"
    mv -f "$stage" "$destination" || fail "could not install $destination"
    stage=
}

# Validate a staged executable on the destination filesystem. This supports
# hardened systems where the temporary directory itself is mounted noexec,
# while leaving any existing agsh executable untouched until validation passes.
[ ! -d "$INSTALL_DIR/agsh" ] ||
    fail "refusing to replace directory destination: $INSTALL_DIR/agsh"
binary_stage=$(mktemp "$INSTALL_DIR/.agsh.XXXXXX") ||
    fail "could not create executable staging file in $INSTALL_DIR"
install -m 755 "$extract/$name/agsh" "$binary_stage" ||
    fail "could not stage agsh executable"
reported_version=$("$binary_stage" --version) ||
    fail "downloaded agsh binary could not run on this platform"
[ "$reported_version" = "agsh ${VERSION#v}" ] ||
    fail "downloaded binary version does not match $VERSION (got: $reported_version)"

# Publish the executable last so an interrupted upgrade cannot expose a new
# binary before its matching optional library and notices are in place.
install_atomic "$extract/$name/$intercept_lib" "$INSTALL_DIR/$intercept_lib" 755
install_atomic "$extract/$name/LICENSE" "$DOC_DIR/LICENSE" 644
install_atomic "$extract/$name/NOTICE" "$DOC_DIR/NOTICE" 644
install_atomic "$extract/$name/THIRD_PARTY_LICENSES.html" "$DOC_DIR/THIRD_PARTY_LICENSES.html" 644
install_atomic "$extract/$name/RUST_STANDARD_LIBRARY_COPYRIGHT.html" \
    "$DOC_DIR/RUST_STANDARD_LIBRARY_COPYRIGHT.html" 644
[ -z "$platform_copyright" ] || install_atomic \
    "$extract/$name/$platform_copyright" "$DOC_DIR/$platform_copyright" 644
install_atomic "$extract/$name/LICENSES/Apache-2.0.txt" "$DOC_DIR/Apache-2.0.txt" 644
[ ! -d "$INSTALL_DIR/agsh" ] ||
    fail "refusing to replace directory destination: $INSTALL_DIR/agsh"
mv -f "$binary_stage" "$INSTALL_DIR/agsh" || fail "could not install agsh"
binary_stage=

installed_version=$("$INSTALL_DIR/agsh" --version) ||
    fail "installed agsh binary could not run from $INSTALL_DIR"
[ "$installed_version" = "$reported_version" ] ||
    fail "installed agsh version changed unexpectedly (got: $installed_version)"
say "Installed $installed_version -> $INSTALL_DIR/agsh"
say "License notices -> $DOC_DIR"
case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
    say ""
    say "NOTE: $INSTALL_DIR is not on your PATH. Add it to your shell profile:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
say "Run 'agsh' to start. Docs: https://github.com/$REPO#readme"
