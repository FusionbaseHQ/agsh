#!/usr/bin/env sh
# Interactive (PTY) tests — needs a working pseudo-terminal. Not part of the
# main `check.sh` gate (timing-sensitive); run locally / on demand.
set -eu
cargo build
python3 tests/interactive/run.py
