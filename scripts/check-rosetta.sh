#!/usr/bin/env sh
# Exercise the published x86_64 helper/interposer while it hands off to an
# arm64-only target. This is the loader transition that generic unit tests
# cannot reproduce from a native process.
set -eu
umask 077

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    printf '%s\n' 'Rosetta compatibility: skipped (requires Apple-silicon macOS)'
    exit 0
fi

if ! rustup target list --installed | grep -F -x -q x86_64-apple-darwin; then
    if [ "${AGSH_REQUIRE_ROSETTA_CHECK:-0}" = 1 ]; then
        printf '%s\n' 'Rosetta compatibility: x86_64-apple-darwin Rust target is not installed' >&2
        exit 1
    fi
    printf '%s\n' 'Rosetta compatibility: skipped (x86_64 Rust target not installed)'
    exit 0
fi

if ! /usr/bin/arch -x86_64 /usr/bin/true >/dev/null 2>&1; then
    if [ "${AGSH_REQUIRE_ROSETTA_CHECK:-0}" = 1 ]; then
        printf '%s\n' 'Rosetta compatibility: Rosetta is unavailable' >&2
        exit 1
    fi
    printf '%s\n' 'Rosetta compatibility: skipped (Rosetta unavailable)'
    exit 0
fi

cd "$ROOT"
cargo build --locked -p agsh -p agsh-intercept
cargo build --locked -p agsh -p agsh-intercept --target x86_64-apple-darwin
cargo test --locked -p agsh --target x86_64-apple-darwin raw_exec

native_bin="$ROOT/target/debug/agsh"
x86_dir="$ROOT/target/x86_64-apple-darwin/debug"
x86_bin="$x86_dir/agsh"
x86_helper="$x86_dir/agsh-exec-helper"
x86_interposer="$x86_dir/libagsh_intercept.dylib"
test "$(lipo -archs "$native_bin")" = arm64
test "$(lipo -archs "$x86_bin")" = x86_64
test "$(lipo -archs "$x86_helper")" = x86_64
test "$(lipo -archs "$x86_interposer")" = x86_64

probe_dir=$(mktemp -d "${TMPDIR:-/tmp}/agsh-rosetta-check.XXXXXX")
trap 'rm -rf "$probe_dir"' EXIT INT TERM
mkdir "$probe_dir/home" "$probe_dir/tmp"
stdout="$probe_dir/stdout"
stderr="$probe_dir/stderr"
source='unset AGSH_SELF; "$AGSH_ROSETTA_TARGET" --version'

if ! env -i \
    PATH=/usr/bin:/bin \
    HOME="$probe_dir/home" \
    TMPDIR="$probe_dir/tmp" \
    XDG_CONFIG_HOME="$probe_dir/config" \
    XDG_DATA_HOME="$probe_dir/data" \
    XDG_STATE_HOME="$probe_dir/state" \
    AGSH_HISTORY_FILE="$probe_dir/history" \
    AGSH_TRUST_FILE="$probe_dir/trust" \
    AGSH_SESSION_DIR="$probe_dir/sessions" \
    AGSH_BROKER_DIR="$probe_dir/broker" \
    AGSH_TRACE_DIR="$probe_dir/traces" \
    AGSH_NORC=1 \
    AGSH_INTERCEPT=semantic:deep \
    AGSH_ROSETTA_TARGET="$native_bin" \
    /usr/bin/arch -x86_64 "$x86_bin" --output raw -c "$source" \
    >"$stdout" 2>"$stderr"
then
    cat "$stderr" >&2
    printf '%s\n' 'Rosetta compatibility: x86_64-to-arm64 handoff failed' >&2
    exit 1
fi

test ! -s "$stderr" || {
    cat "$stderr" >&2
    printf '%s\n' 'Rosetta compatibility: unexpected stderr' >&2
    exit 1
}
expected_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
test "$(cat "$stdout")" = "agsh $expected_version"
printf '%s\n' 'Rosetta compatibility: ok (x86_64 agsh -> arm64 target)'
