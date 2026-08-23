# Implementation plan and current status

This document is the status-oriented implementation contract for `agsh`. It is
deliberately conservative: a feature is not called production-ready merely
because types or command-line syntax exist for it.

## Release stance

`agsh` 0.2.0 is the first supported public **pre-1.0 preview**. The local
interactive shell, command executor, output layer, and keep broker have broad
automated coverage on macOS and Ubuntu, but the project does not claim complete
POSIX/Bash compatibility or general production maturity. Optional capabilities
that are not part of this release remain explicitly scoped below: Linux kernel
confinement and the authenticated agent server are not implemented. See
[SECURITY_MODEL.md](SECURITY_MODEL.md) for the security boundary.

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
- External launch uses the small, version-coupled `agsh-exec-helper` sibling
  for a raw `execve` handoff, preventing libc from silently reinterpreting an
  ENOEXEC image as shell source across agsh-managed direct spawn, PTY, pipeline,
  `exec`, snapshot, session-resume, and kept-job routes.
  The kernel always receives the target first, so custom executable handlers
  retain precedence. Only after a real ENOEXEC does the helper make a bounded
  4 KiB regular-file probe and explicitly invoke `/bin/sh` for executable
  text; native-image magic, malformed shebangs, binary or inconclusive first
  lines, unreadable files, and files observed as special during the probe fail
  with 126. This costs one small helper exec per external launch in exchange for
  byte-exact argv/fd behavior, ordinary exported target environments, and
  cross-platform launch consistency; releases install the helper beside agsh,
  while copied development binaries fall back to agsh's identical private mode.
  External launching requires the OS to report agsh's current executable path;
  unusual Linux chroots/containers without a usable `/proc/self/exe` may start
  the shell but reject external commands as an unconfigured-helper error.
  For the `exec` builtin, errors resolved before the handoff leave an interactive
  shell running as usual. A target that fails only after agsh has successfully
  replaced itself with the helper (for example, a post-resolution disappearance,
  malformed image, or missing shebang interpreter) exits 126/127 instead of
  returning to that shell; this rare state-losing divergence remains pre-1.0
  compatibility work.
  On macOS, `DYLD_*` target bindings cross hardened helper and kept-job
  supervisor boundaries through a private encoded environment namespace that
  is removed before the target starts; release gates exercise that boundary
  with hardened ad-hoc signatures before Developer ID signing.
  `AGSH_INTERNAL_EXEC_DYLD_V1_*` is reserved for that private macOS transport
  and is removed from target environments; applications must not use the prefix.
  This does not rewrite the semantics of an explicitly invoked interpreter.
  In particular, the current macOS strict-confinement backend enters its bounded
  sandbox through `/bin/sh -c`; commands nested inside that interpreter retain
  its ENOEXEC fallback behavior, but remain subject to the sandbox policy.
- The `pty`/`agpty` wrapper currently captures output only. It does not forward
  interactive input; piped or fd-0 redirected input is rejected with status 2
  before the payload starts instead of being ignored or allowed to hang.
- Background command-list items inherit scalar variables, arrays, aliases,
  functions, variable attributes, and shell options through an acknowledged,
  bounded, length-framed Unix-socket handoff; no secret-bearing temporary file
  is created and startup does not depend on socket EOF delivery.
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
  EOF, so a long-lived descendant can keep up to two workers alive. A shell-wide
  ceiling reserves one of 64 admissions when each capture reader is created and
  transfers that reservation to its acknowledged helper. Saturated capture
  therefore fails explicitly during setup, with spawned pipeline stages reaped
  and no complete trace published, rather than waiting after execution or
  closing an admitted live pipe. This globally bounded but process-heavy
  lifecycle is accepted only as a pre-1.0 limitation.
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
- Broker control requests and per-job log generations are bounded. The daemon
  admits at most 64 running jobs, retains 20 finished records, deletes logs on
  record pruning/removal, and keeps at most 20 prior-generation job IDs / 128
  MiB after its generation-locked startup sweep. The daemon log rotates at 1
  MiB with one old generation; its serialized accept loop performs runtime
  rotation/reopen checks. Same-UID peers and unredacted broker/session files
  remain inside the trust boundary.
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
