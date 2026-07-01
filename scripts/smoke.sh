#!/usr/bin/env sh
set -eu
cargo run -p agsh -- -c 'echo hello'
cargo run -p agsh -- -c "FOO=bar sh -c 'printf %s \"\$FOO\"'"
cargo run -p agsh -- --output semantic -c "sh -c 'echo error: sample failure >&2'"
