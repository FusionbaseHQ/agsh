# Testing strategy

Correctness for a shell is behavioral. Tests must cover parser/state logic and
the real binary, and must compare observable bytes, exit codes, filesystem
effects, signals, and terminal behavior where applicable.

## Required local gates

```sh
scripts/check.sh
scripts/check-interactive.sh
```

`scripts/check.sh` runs release/installer validation, `cargo fmt --check`, all
workspace tests, Clippy with warnings denied, a locked build, golden checks, the
Bash differential suite, and the POSIX `sh` differential suite. The interactive
script builds the binary and runs the PTY emulator suite separately. On
Apple-silicon macOS, the main check also runs x86_64 raw-exec unit tests under
Rosetta and verifies an x86_64 agsh can hand off to an arm64-only target; CI and
release gates require the cross target instead of silently skipping it.

## Test layers

1. Crate unit tests own pure lexer, parser, expansion, resolver, state, policy,
   output, storage, rendering, and protocol behavior.
2. `crates/agsh/tests/exec.rs` drives the real binary for command/script/piped
   modes, environment semantics, processes, redirects, output modes, and
   regression cases.
3. `tests/checks/*.agsh` are littlecheck-style golden scripts. Directives include
   `RUN`, `REQUIRES`, `CHECK`, `CHECKERR`, and `CHECKEXIT`.
4. `tests/differential/diff.py` compares stdout, exit status, stderr presence,
   and filesystem snapshots with Bash. Intentional differences require a narrow
   explanation in `EXPECTED_DIFFS`.
5. `tests/differential/posix.py` contains POSIX-only cases and compares stdout
   plus status with `sh` (or `REF=dash`).
6. `tests/interactive/run.py` uses a PTY and terminal emulator for line editing,
   completion, history, rendering, signals, and bounded-screen regressions.
7. `tests/release/run.sh` checks stable-tag policy and the workflow's final tag,
   immutability, and network-isolation guards without contacting GitHub.
8. `tests/install/run.sh` exercises the installer offline with fake network and
   platform tools, including checksum, archive, symlink, and version failures.
   With no argument it runs explicit `linux-x86_64` and `darwin-arm64` fixtures;
   The CI and release gates map those fixtures to their Ubuntu and macOS runners
   without consulting the host's real `uname` for target selection.

## Isolation rules

Every test that executes `agsh` must use a temporary `HOME`, XDG directories,
history/trust/session paths, and working directory. It must remove inherited
`AGSH_OUTPUT_MODE`, interception, rc, icon/color, and similar state unless the
test intentionally sets it. Tests must never inspect or mutate the developer's
real history, config, credentials, session journals, or trust database.

Use portable executables (`echo`, `printf`, `pwd`, `true`, `false`, `cat`, and
`sh -c`) in cross-platform tests. Put GNU/macOS-specific behavior behind
`REQUIRES` or platform-specific Rust test guards. Never make a network call in a
deterministic test.

## Regression ownership

Every bug fix needs a test at the lowest layer that reproduces the failure:

- token/segment/AST bug: lexer/parser unit test;
- expansion or shell-state bug: executor/state unit test;
- child process, environment, fd, pipe, or redirect bug: binary integration test;
- compatibility mismatch: differential case;
- stable user-facing rendering: golden case;
- editor, PTY, resize, signal, or broker attach behavior: PTY/broker test;
- release/installer bug: offline shell fixture or release validator.

Tests for raw semantics compare bytes rather than lossy UTF-8 strings. Cover
non-UTF-8 argv/environment/files where the platform permits it. Timeout every
child/PTY/network-like operation and bound generated input so a regression
cannot wedge CI indefinitely.

## Required invariants

- Raw pipes, redirects, files, and child streams are byte-exact.
- Observation compaction never changes command exit status.
- Elided observations retain usable exact-stream references for the documented
  bounded lifetime.
- Export/local/temporary assignment semantics and opaque Unix environment values
  survive child launch correctly.
- PATH changes invalidate executable resolution.
- Project-local config and `.env` files do not load without explicit trust.
- Unsupported confinement fails closed unless `--best-effort` is explicit.
- Protocol/config/file inputs reject oversize, malformed, symlinked, or special
  file targets before unbounded reading or mutation.

## Remaining coverage gaps

The current suites do not establish full POSIX/Bash conformance, Linux kernel
sandbox safety, authenticated agent-server safety, bit-for-bit reproducible
release builds, or compatibility with every advertised terminal/macOS version.
The project also lacks checked-in coverage-guided fuzzing and benchmark
regression gates. These are release-readiness work, not implied by passing the
current suite.
