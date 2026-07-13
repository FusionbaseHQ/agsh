# Implementation plan and current status

This document is the status-oriented implementation contract for `agsh`. It is
deliberately conservative: a feature is not called production-ready merely
because types or command-line syntax exist for it.

## Release stance

`agsh` 0.2.0 is the first public release. The local interactive shell,
command executor, output layer, and keep broker have broad automated coverage on
macOS and Ubuntu. Optional capabilities that are not part of this release remain
explicitly scoped below: Linux kernel confinement and the authenticated agent
server are not implemented. See [SECURITY_MODEL.md](SECURITY_MODEL.md) for the
security boundary.

## Phase status

| Phase | Status | What exists | What remains before the phase is complete |
| --- | --- | --- | --- |
| 0: robust scaffold | Implemented | Workspace builds; lexer/parser, environment, builtins, output, and CLI integration tests | Keep every supported platform gate green |
| 1: command compatibility | Implemented with gaps | Resolver, PATH lookup/cache, `type`, `which`, `command`, `external`, and `builtin` | Continue differential compatibility work |
| 2: expansion semantics | Substantial | Quoted segments, parameters, command/arithmetic substitution, brace/tilde/glob expansion, arrays, and many Bash extensions | POSIX certification and full Bash compatibility are not claimed |
| 3: processes and terminal | Substantial | Pipelines, redirects, background jobs, signals, PTYs, and the persistent keep broker | More adversarial descriptor/job-control coverage and cross-platform soak testing |
| 4: token-economy output | Implemented for foreground capture with bounded retention | Seven output modes, exact raw stream separation, capped disk traces, semantic compactors, TOML rules, and budgets | Add ordered durable async capture; enforce the currently declarative duration-based retention setting |
| 5: agent protocol | Schema/codec only | Bounded JSONL codec and session path model | Authenticated server, operation dispatch, cancellation, ownership, Unix socket, approval store, and MCP adapter |
| 6: security and sandboxing | Partial, macOS only | Deterministic risk/policy logic and fail-closed macOS Seatbelt presets | Linux Landlock/namespaces/seccomp/cgroups/network backend; approval persistence; minimal agent environment; adversarial isolation suite |
| 7: terminal UX/performance | Partial | Native editor, history/completion, rendering, project index, session journal, and broker | Formal startup/latency budgets, benchmark regression gates, and wider terminal compatibility testing |

## Known incompatibilities and unsupported scope

- The differential suite intentionally records the remaining Bash divergence in
  `tests/differential/diff.py`; new differences must not be silently accepted.
- Native mode implements a large POSIX/Bash-inspired surface, not every Bash,
  zsh, fish, or POSIX edge case. Run important scripts with their declared
  interpreter until they pass the differential/golden suites under `agsh`.
- Indexed arrays currently use dense storage, so unsetting an element shifts
  later numeric indices instead of preserving Bash-style sparse indices.
- `declare` supports common attributes, but `+x`/`+r`, advanced function flags
  and filtering are incomplete; `declare -g` also cannot yet bypass a same-name
  local variable.
- Crash-journal replay does not preserve exported-but-unset state or integer
  attributes.
- Session-journal appends are bounded and tolerate malformed records, but are
  not synchronously flushed at every command boundary; abrupt power loss can
  lose recent deltas that had not reached durable storage.
- POSIX fatal-shell behavior for assignment errors on special builtins is not
  modeled; diagnostics/status can differ without terminating the shell.
- Redirections involving descriptors above `2` are rejected before execution.
  Standard stdin/stdout/stderr file, append, close, and `2>&1`/`1>&2` forms are
  supported; arbitrary descriptor mapping needs a dedicated safe spawn helper.
- Background command-list items inherit scalar variables, arrays, aliases,
  functions, variable attributes, and shell options through an acknowledged,
  bounded Unix-socket handoff; no secret-bearing temporary file is created.
  Native typed `Value` variants outside the shell variable model are restored
  through their scalar representation.
- A parsed top-level graph containing an asynchronous-list `&` operator forces
  raw output for the whole graph, even when a capturing mode was requested.
  This preserves launch-time descriptor routing, byte chronology, status, and
  one-shot child lifetime, but temporarily disables compact/semantic/lossless
  observations for such graphs. Async introduced dynamically by `eval`,
  `source`, or a previously defined function is not visible to this outer guard.
  When an external command hides a detached descendant, capture cutoff hands
  retained read descriptors to a detached fixed-buffer drain helper so later
  writes remain viable; post-cutoff bytes are discarded and traces are marked
  incomplete rather than replayed. These are safety drains, not async output
  collectors: each retained stdout/stderr stream owns one worker until kernel
  EOF, so a long-lived descendant can keep up to two workers alive. This bounded
  per-command but process-heavy lifecycle is accepted only as a pre-1.0
  limitation. There is no global admission bound: repeated indefinitely
  retaining descendants can create an unbounded aggregate number of workers.
  Before advertising structured async capture, replace this fallback with a
  graph/session-wide ordered event spool that records each byte range with its
  launch-time logical destination, backed by a detached internal collector for
  post-parent lifetime.
  The collector must write private bounded data/metadata files, record
  direct-child status and truncation, and attach durable job-log references to
  the parent observation without injecting text into command stdout/stderr.
- `jobs`, `wait`, and process-group signaling are implemented, including PID
  operands and signal exit status. Full interactive terminal job control is not:
  `fg`/`bg` do not yet perform `tcsetpgrp` handoff or complete stopped-process
  tracking, so they must not be presented as mature Bash-equivalent job control.
- Input process substitution (`<(...)`) is synchronous and private-temp-file
  backed. It preserves finite output exactly, but does not provide Bash's
  concurrent `/dev/fd` behavior; it fails explicitly if the shared raw-capture
  quota cannot retain the complete stream.
- Linux `confine` has no kernel backend and refuses to run unless the caller
  explicitly requests the non-security `--best-effort` shim.
- The agent protocol crate is not a remotely exposable command server.
- Deep `LD_PRELOAD`/`DYLD_INSERT_LIBRARIES` interception is experimental,
  incomplete, and never a security boundary.
- PATH interception/confinement shims are built as fresh owner-private
  generations with exclusive files. Successful generations are retained so
  already-running children that inherited their PATH keep working after a mode
  toggle; bounded stale-generation cleanup is not implemented yet.
- Trace recovery is bounded: the in-session index defaults to 200 commands and
  persisted traces default to 512 files with a hard 2 GiB directory ceiling.
  Old references can expire.
- Broker control requests and per-job log generations are bounded, but there is
  no aggregate cap on running jobs, accumulated old job logs, or the daemon log.
  Same-UID peers and unredacted broker/session files remain inside the trust
  boundary; external cleanup is required for long-lived broker directories.
- `snapshot` checkpoints tracked working-tree content only. Untracked/ignored
  files are excluded, restore overwrites tracked files below the cwd, and Git
  subprocess capture/time limits can make the operation fail explicitly.
- `[storage] max_raw_per_command` is a shared stdout/stderr ceiling and
  `store_raw = false` suppresses persistence. `[storage] raw_retention` remains
  reserved; persisted traces are currently retained by bounded file count.
- macOS confinement relies on the platform's deprecated `sandbox-exec` surface;
  it needs regression testing on every OS version the project advertises.

## Production priorities

1. Keep raw pipes, redirects, files, and child streams byte-exact under load and
   add a regression for every discovered corruption/deadlock case.
2. Close documented parser/executor differences and expand descriptor, signal,
   TTY, non-UTF-8, and resource-exhaustion tests.
3. Implement and adversarially test Linux kernel confinement before advertising
   strict security on Ubuntu.
4. Build the authenticated agent operation server only after capability
   derivation, minimal environments, trace authorization, approvals, and bounded
   streaming are enforceable end to end.
5. Add reproducible performance budgets and supported-OS compatibility runs.

## Required gates

Run `scripts/check.sh` for formatting, Rust tests, Clippy, build, golden tests,
and both differential suites. Run `scripts/check-interactive.sh` for the local
PTY suite. Tagged releases additionally run the PTY suite on GitHub-hosted
macOS and Ubuntu runners, build all four native release targets, validate the
installer, sign/notarize macOS artifacts, and publish checksummed attestations.

Detailed test ownership and isolation rules are in
[TESTING_STRATEGY.md](TESTING_STRATEGY.md). Output invariants and configuration
status are in [TOKEN_ECONOMY.md](TOKEN_ECONOMY.md).
