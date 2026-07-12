# Contributing to `agsh`

Thanks for your interest! `agsh` is an early-stage project; contributions,
bug reports, and ideas are welcome.

## Development setup

```sh
git clone https://github.com/FusionbaseHQ/agsh.git && cd agsh
cargo build
```

## Before you open a PR

Run the full local check suite — CI runs the same on Linux and macOS:

```sh
cargo fmt --all
scripts/validate-release.sh
tests/install/run.sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

# Behavioral suites:
python3 tests/checks/run.py tests/checks/*.agsh   # golden output checks
python3 tests/differential/diff.py                # parity vs bash
python3 tests/differential/posix.py               # parity vs sh
python3 tests/interactive/run.py                  # PTY editor/completion/render
```

## Ground rules

- **No `unsafe`** in first-party crates — the workspace forbids it (dependencies may
  use it). The sole exception is `agsh-intercept`, the optional preload interposer,
  which needs libc FFI and is deliberately isolated; don't add `unsafe` elsewhere.
- **Never corrupt raw streams.** Pipes and redirects must receive exact bytes;
  rich rendering and native accelerations are opt-in and TTY-gated.
- **Every bug fix adds a regression test.**
- Keep behavior parity with `bash`/`sh` for standard shell semantics; document any
  intentional divergence.
- Match the style and comment density of the surrounding code.

## Licensing of contributions

`agsh` is licensed under the **GNU AGPL-3.0** (see [`LICENSE`](LICENSE)). By
submitting a contribution, you agree that it is licensed under the same terms.
