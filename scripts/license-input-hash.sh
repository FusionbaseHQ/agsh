#!/usr/bin/env sh
# Digest every input that can change the dependency/license graph or rendering.
set -eu
export LC_ALL=C

inputs="Cargo.lock Cargo.toml about.toml about.hbs scripts/license-input-hash.sh"
for manifest in crates/*/Cargo.toml; do
    inputs="$inputs $manifest"
done

if command -v sha256sum >/dev/null 2>&1; then
    {
        for file in $inputs; do
            printf '\nFILE:%s\n' "$file"
            cat "$file"
        done
    } | sha256sum | awk '{print $1}'
else
    {
        for file in $inputs; do
            printf '\nFILE:%s\n' "$file"
            cat "$file"
        done
    } | shasum -a 256 | awk '{print $1}'
fi
