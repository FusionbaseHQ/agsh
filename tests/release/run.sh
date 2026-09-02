#!/usr/bin/env sh
# Offline regression checks for release metadata and workflow fail-closed guards.
set -eu
umask 077

ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/agsh-release-test.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

for manifest in "$ROOT"/crates/*/Cargo.toml; do
    grep -F -x -q 'rust-version.workspace = true' "$manifest" || {
        printf 'release tests: %s does not inherit workspace rust-version\n' "$manifest" >&2
        exit 1
    }
done

for tag in v0.2.0-rc.1 v0.2.0+build.1; do
    if (cd "$ROOT" && scripts/validate-release.sh "$tag") \
        >"$TMP/out" 2>"$TMP/err"
    then
        printf 'release tests: unsupported tag was accepted: %s\n' "$tag" >&2
        exit 1
    fi
    grep -F -q 'tag must be a stable vMAJOR.MINOR.PATCH release tag' "$TMP/err"
done

if (cd "$ROOT" && scripts/validate-release.sh v9.9.9) \
    >"$TMP/stable.out" 2>"$TMP/stable.err"
then
    printf '%s\n' 'release tests: mismatched stable tag was accepted' >&2
    exit 1
fi
grep -F -q 'does not match workspace version' "$TMP/stable.err"

workflow="$ROOT/.github/workflows/release.yml"
[ "$(grep -F -c 'if [ "${{ github.event.repository.private }}" != false ]; then' "$workflow")" -eq 1 ]
grep -F -q -- 'public release workflow requires the repository to remain public' "$workflow"
grep -F -q -- "if: github.event_name != 'push' || github.event.deleted == false" "$workflow"
[ "$(grep -F -c "if: github.event_name == 'push' && github.event.deleted == false && startsWith(github.ref, 'refs/tags/v') && github.event.repository.private == false" "$workflow")" -eq 2 ]
[ "$(grep -c '^[[:space:]]*verify_release_tag_binding$' "$workflow")" -eq 3 ]
[ "$(grep -c '^[[:space:]]*verify_repository_public$' "$workflow")" -eq 3 ]
grep -F -q -- 'gh api "repos/$GITHUB_REPOSITORY" --jq .visibility' "$workflow"
grep -F -q -- 'test "$target_sha" = "$GITHUB_SHA"' "$workflow"
grep -F -q -- '--draft' "$workflow"
grep -F -q -- 'gh release edit "$GITHUB_REF_NAME" --draft=false --latest' "$workflow"
awk '
    /^[[:space:]]*verify_release_tag_binding$/ {
        previous_guard = NR
        next
    }
    /^[[:space:]]*verify_repository_public$/ {
        if (previous_guard == NR - 1) {
            public_guard = NR
        }
        next
    }
    /gh release edit "\$GITHUB_REF_NAME" --draft=false --latest/ {
        if (public_guard != NR - 1) {
            exit 1
        }
        found = 1
    }
    END { if (!found) exit 1 }
' "$workflow" || {
    printf '%s\n' 'release tests: tag/public guards are not immediately before draft publication' >&2
    exit 1
}
grep -F -q -- '"repos/$GITHUB_REPOSITORY/releases/tags/$GITHUB_REF_NAME"' "$workflow"
grep -F -q -- '--jq .immutable' "$workflow"
grep -F -q -- 'sudo --non-interactive "$unshare_bin" --net -- "$setpriv_bin"' "$workflow"
grep -F -q -- '--reuid "$runner_uid" --regid "$runner_gid" --init-groups' "$workflow"
grep -F -q -- '--no-new-privs --' "$workflow"
grep -F -q -- 'CARGO_NET_OFFLINE=true' "$workflow"
grep -F -q -- 'cargo_bin=$(rustup which --toolchain "$toolchain" cargo)' "$workflow"
grep -F -q -- 'rustc_bin=$(rustup which --toolchain "$toolchain" rustc)' "$workflow"
grep -F -q -- '"PATH=$toolchain_bin:$PATH"' "$workflow"
grep -F -q -- '"RUSTC=$rustc_bin"' "$workflow"
grep -F -q -- 'RUSTUP_AUTO_INSTALL=0' "$workflow"
grep -F -q -- '"$cargo_bin" build --workspace --release --locked --offline' "$workflow"
if grep -F -q -- 'cargo_bin=$(command -v cargo)' "$workflow"; then
    printf '%s\n' 'release tests: source verifier invokes the rustup cargo proxy' >&2
    exit 1
fi
grep -F -q -- "if: matrix.target == 'aarch64-unknown-linux-musl'" "$workflow"
grep -F -q -- 'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc' "$workflow"
grep -F -q -- "if [ \"\${{ runner.os }}\" = Linux ]; then" "$workflow"
grep -F -q -- "grep -q ' INTERP ' \"\$program_headers\"" "$workflow"
grep -F -q -- "grep -q '(NEEDED)' \"\$dynamic_entries\"" "$workflow"
grep -F -q -- 'test "$("$bin" --version)" = "agsh $expected_version"' "$workflow"
grep -F -q -- 'Smoke test hardened macOS helper boundary' "$workflow"
grep -F -q -- 'AGSH_INTERNAL_EXEC_DYLD_V1_' "$workflow"
grep -F -q -- 'test "$result" = hardened-transport-ok' "$workflow"
grep -F -q -- "otool -arch arm64e -hv /usr/bin/wc" "$workflow"
grep -F -q -- "system_wc_result=" "$workflow"
grep -F -q -- 'cc -arch x86_64 -dynamiclib' "$workflow"
grep -F -q -- 'test "$cross_arch_result" = cross-arch-preload-ok' "$workflow"
grep -F -q -- 'test "$(cat "$cross_arch_marker")" = caller-loaded' "$workflow"
if grep -F -q -- 'caller-loaded\\n' "$workflow"; then
    printf '%s\n' 'release tests: generated C fixture writes a literal backslash-n marker' >&2
    exit 1
fi
if grep -F -q -- 'cc -arch arm64e -dynamiclib' "$workflow"; then
    printf '%s\n' 'release tests: release smoke still builds a non-platform arm64e preload fixture' >&2
    exit 1
fi
grep -F -q -- 'AGSH_REQUIRE_ROSETTA_CHECK=1 scripts/check-rosetta.sh' "$workflow"
grep -F -q -- 'scripts/check-rosetta.sh' "$ROOT/scripts/check.sh"
if grep -F -q -- 'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER' "$workflow" ||
    grep -F -q -- 'CARGO_TARGET_$(echo' "$workflow"
then
    printf '%s\n' 'release tests: x86_64 musl build forces the broken musl-gcc PIE path' >&2
    exit 1
fi

guard="$TMP/verify-release-tag-binding.sh"
awk '
    index($0, "cat > \"$RUNNER_TEMP/verify-release-tag-binding.sh\"") {
        inside = 1
        next
    }
    inside && /^[[:space:]]*BASH$/ { exit }
    inside {
        sub(/^          /, "")
        print
    }
' "$workflow" >"$guard"
test -s "$guard"
bash -n "$guard"

cat >"$TMP/gh" <<'EOF'
#!/usr/bin/env sh
set -eu
[ "$1" = api ]
endpoint=$2
case "$endpoint" in
repos/FusionbaseHQ/agsh)
    printf '%s\n' "$FAKE_GH_VISIBILITY"
    ;;
*/git/ref/tags/*)
    count=0
    [ ! -f "$FAKE_GH_COUNT" ] || count=$(cat "$FAKE_GH_COUNT")
    count=$((count + 1))
    printf '%s\n' "$count" >"$FAKE_GH_COUNT"
    if [ "$FAKE_GH_MODE" = moved ] && [ "$count" -gt 1 ]; then
        printf 'tag\ttag-object-moved\n'
    else
        printf 'tag\ttag-object\n'
    fi
    ;;
*/git/tags/tag-object)
    if [ "$FAKE_GH_MODE" = wrong-target ]; then
        printf 'commit\tbad-commit\n'
    else
        printf 'commit\texpected-commit\n'
    fi
    ;;
*) exit 65 ;;
esac
EOF
chmod 755 "$TMP/gh"

run_guard() {
    mode=$1
    visibility=${2:-public}
    rm -f "$TMP/gh.count"
    PATH="$TMP:$PATH" \
        FAKE_GH_COUNT="$TMP/gh.count" \
        FAKE_GH_MODE="$mode" \
        FAKE_GH_VISIBILITY="$visibility" \
        GITHUB_REPOSITORY=FusionbaseHQ/agsh \
        GITHUB_REF_NAME=v0.2.0 \
        GITHUB_SHA=expected-commit \
        bash -c 'set -e; . "$1"; verify_repository_public; verify_release_tag_binding' _ "$guard"
}

run_guard success
if run_guard wrong-target >"$TMP/wrong.out" 2>"$TMP/wrong.err"; then
    printf '%s\n' 'release tests: wrong tag target was accepted' >&2
    exit 1
fi
grep -F -q 'moved from workflow commit' "$TMP/wrong.out"

if run_guard moved >"$TMP/moved.out" 2>"$TMP/moved.err"; then
    printf '%s\n' 'release tests: tag movement during validation was accepted' >&2
    exit 1
fi
grep -F -q 'changed while its target was being checked' "$TMP/moved.out"

if run_guard success private >"$TMP/private.out" 2>"$TMP/private.err"; then
    printf '%s\n' 'release tests: publication guard accepted a private repository' >&2
    exit 1
fi
grep -F -q 'repository visibility changed from public' "$TMP/private.out"

printf '%s\n' 'release tests: ok'
