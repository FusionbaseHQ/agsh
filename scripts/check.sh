#!/usr/bin/env sh
set -eu
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build
echo "== golden checks =="
python3 tests/checks/run.py
echo "== differential (vs ${REF:-bash}) =="
python3 tests/differential/diff.py
echo "== POSIX differential (vs sh) =="
python3 tests/differential/posix.py
