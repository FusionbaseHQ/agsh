#!/bin/sh
# agsh installer — https://github.com/FusionbaseHQ/agsh
#
#   curl -fsSL https://raw.githubusercontent.com/FusionbaseHQ/agsh/main/install.sh | sh
#
# Downloads the prebuilt binary for this platform from GitHub Releases,
# verifies its sha256 against the release's checksums.txt, and installs it.
# No sudo needed. Environment overrides:
#
#   AGSH_VERSION      release tag to install (default: latest, e.g. v0.2.0)
#   AGSH_INSTALL_DIR  install directory (default: ~/.local/bin)
#
# Prefer to read before you run? Download first:
#   curl -fsSLO https://raw.githubusercontent.com/FusionbaseHQ/agsh/main/install.sh
#   less install.sh && sh install.sh

set -eu

REPO="FusionbaseHQ/agsh"
INSTALL_DIR="${AGSH_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${AGSH_VERSION:-latest}"

say() { printf '%s\n' "$*" >&2; }
fail() {
    say "agsh-install: error: $*"
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

# ---- platform detection -> Rust target triple -------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
Darwin)
    case "$arch" in
    arm64 | aarch64) target="aarch64-apple-darwin" ;;
    x86_64) target="x86_64-apple-darwin" ;;
    *) fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
Linux)
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
    effective=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest") ||
        fail "cannot reach github.com to resolve the latest release"
    VERSION=${effective##*/}
    case "$VERSION" in
    v*) ;;
    *) fail "could not resolve the latest release tag (got: $VERSION)" ;;
    esac
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac

name="agsh-$VERSION-$target"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# ---- download + verify -------------------------------------------------------
say "Downloading agsh $VERSION ($target)…"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" ||
    fail "download failed: $base/$name.tar.gz"
curl -fsSL "$base/checksums.txt" -o "$tmp/checksums.txt" ||
    fail "could not download checksums.txt for verification"

expected=$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$tmp/checksums.txt")
[ -n "$expected" ] || fail "no checksum entry for $name.tar.gz in checksums.txt"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$name.tar.gz" | awk '{print $1}')
else
    actual=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] ||
    fail "checksum mismatch for $name.tar.gz
  expected: $expected
  actual:   $actual
The download may be corrupted or tampered with — not installing."

# ---- install -----------------------------------------------------------------
tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
[ -f "$tmp/$name/agsh" ] || fail "archive did not contain the agsh binary"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$name/agsh" "$INSTALL_DIR/agsh"

say "Installed $("$INSTALL_DIR/agsh" --version) -> $INSTALL_DIR/agsh"
case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
    say ""
    say "NOTE: $INSTALL_DIR is not on your PATH. Add it to your shell profile:"
    say "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
say "Run 'agsh' to start. Docs: https://github.com/$REPO#readme"
