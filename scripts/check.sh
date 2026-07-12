#!/usr/bin/env sh
set -eu
scripts/validate-release.sh
tests/release/run.sh
tests/install/run.sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --locked
echo "== golden checks =="
python3 tests/checks/run.py
echo "== differential (vs ${REF:-bash}) =="
python3 tests/differential/diff.py
echo "== POSIX differential (vs sh) =="
python3 tests/differential/posix.py
