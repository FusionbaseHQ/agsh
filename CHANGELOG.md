# Changelog

All notable changes to `agsh` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`0.2.0` is the first supported pre-1.0 preview. The obsolete `v0.1.0` tag is
retired with this release; the older changelog section remains only as a record
of the unsupported private development milestone.

## [Unreleased]

## [0.2.0] - 2026-08-26

The first supported preview combines the session-resilience and developer-workflow
features prepared during private development with the production-hardening and
release-integrity work required to ship them.

### Added
- `agenv` builtin: view (`agenv`, `agenv NAME`), set (`agenv NAME=VALUE`,
  `agenv set NAME VALUE`), and unset exported environment variables, plus
  history awareness — `agenv history` lists the newest recorded
  `export`/`agenv` assignment per name and `agenv restore NAME…`/`--all`
  re-applies them; restore selectors and history/list filters also accept
  glob patterns (`agenv restore API_*` re-applies every matching name — no
  quoting needed, since agenv arguments are never pathname-expanded),
  re-expanding values with the shell's own expander at
  restore time (listing and previewing never expand anything). Non-interactive
  shells read the persistent history file without locking or compacting it.
- Forgiving spaced assignments in `export` and `agenv`: `export XYZ = 123`,
  `export XYZ =123`, and `export XYZ= 123` now mean `export XYZ=123` (an agsh
  extension; joining is rescue-only, so every form POSIX shells accept keeps
  its exact meaning).

### Security
- Make kernel confinement fail closed when its backend is unavailable, validate
  private state and trace paths without following symlinks, and remove inherited
  secret-bearing variables from confined child environments.
- Bound agent/broker protocol frames, command capture, process substitution,
  history records, trace files, aggregate trace retention, live capture-drain
  helpers, running kept jobs, retained broker job logs, and daemon-log
  generations.
- Treat truncated, unavailable, or disabled raw storage as incomplete:
  exact-trace APIs refuse partial data and semantic observations distinguish a
  capped trace, a persistence failure, and a configured opt-out.
- Authenticate broker control peers by UID, validate private socket/journal/trust
  paths, replace background-state temp files with an acknowledged, bounded,
  length-framed anonymous handoff, and use versioned SHA-256
  project-environment trust records.
- Build interception and best-effort confinement shims only in validated private
  generations, publish shell state after complete verification, and refuse the
  requested mode when provisioning fails instead of running with partial policy.

### Hardening fixes
- Route agsh-managed direct external launches through a version-coupled
  raw-`execve` helper so malformed executable images cannot become implicit
  shell source; preserve explicit shebang-less text fallback, byte-exact Unix
  arguments, raw pipe/redirection bytes, default SIGPIPE behavior, PTY behavior,
  one raw deep-interception subtree for executable-text fallback without loading
  agsh's interposer into the fallback shell, normal execution of macOS targets
  whose bounded Mach-O header does not advertise compatibility with agsh's
  architecture-specific interposer (including x86_64-to-arm Rosetta handoffs),
  exact preservation of unrelated caller preload bindings, macOS `DYLD_*`
  target bindings across hardened helper/supervisor boundaries, and status 126
  across normal, pipeline, PTY, `exec`, snapshot, session-resume, and kept-job
  routes.
- Resolve PATH executables using effective-ID access checks, continue past an
  inaccessible earlier candidate, revalidate cached paths after permission
  changes, and consistently return 126 for existing non-executable commands.
- Move output wrappers and `agview` into structured executor dispatch so they
  compose inside lists, functions, `eval`, and sourced scripts while pipes and
  redirects retain exact raw bytes; align `agview`/`pty`/`agpty` introspection.
- Treat broker attach/list/status transport failures and inconsistent finished
  state as errors instead of success; cap concurrent kept jobs, prune orphaned
  logs at startup, delete logs with records, and continuously rotate daemon
  diagnostics.
- Make the shipped rc template executable shell syntax, label inactive general
  config/policy files as design references, and isolate every binary integration
  launch from the maintainer's real HOME/XDG/history/trust/session state.
- Use the fixed system Git executable for automatic prompt status probes, while
  explicit Git features continue to resolve the shell's effective `PATH` through
  the guarded external-launch path.
- Reserve one of 64 capture-drain admissions before each captured stream starts
  and transfer it to an acknowledged helper, so saturation fails explicitly
  during setup instead of waiting forever after the direct child exits.
- Correct quoting and expansion across here-documents, parameter operators,
  nested `$@`/`$*`, temporary assignments, exported/read-only variables, and
  associative-array keys; add focused golden and differential regressions.
- Preserve raw child bytes and exit status through pipelines, redirects, command
  substitution, process substitution, and rich/agent observation paths while
  forwarding nested stderr with shell-compatible scope.
- Harden session liveness against PID reuse, make null-command redirection
  failures observable without aborting a command list, and stream shell-internal
  file input instead of reading it into an unbounded buffer.
- Make history append/compaction and trace lookup operate on validated regular
  files with bounded streaming reads, private permissions, and cooperative
  locking; malformed or oversized history entries no longer discard later valid
  records.
- Bound history startup to the newest 64 MiB even for sparse or concurrently
  growing files, keep startup nonblocking when compaction is busy, and serialize
  appends through writer contention instead of silently dropping records.
- Preserve live stdout/stderr routing through compounds, optimized pipelines,
  control structures, substitutions, and background subshells; keep raw nested
  pipelines streaming and cancel shell stages when a later spawn fails.
- Preserve background shell variables, arrays, functions, attributes, options,
  positionals, and active project-environment restoration baselines while
  keeping parent state isolated; make PID waits and signal statuses accurate.
- Bound `read`, builtin `printf`, PTY capture, Git snapshots, trace inspection,
  session journals, and background-state decoding; trace persistence failure no
  longer changes command status or exposes an elided preview as exact output.
- Honor the bounded post-exit drain window for Git helper captures, including a
  final read after a stale nonblocking result, preserving real diagnostics when
  a transient inherited descriptor or reader descheduling delays EOF; keep the
  exited group leader waitable until descendant cleanup is signaled so PID reuse
  cannot redirect that signal to an unrelated process group; normalize inherited
  `SIGCHLD` before child management and refuse to signal an already unreserved ID.
- Anchor relative trace directories across `cd`, route synthesized diagnostics
  through live compound descriptors, and mark retained-descriptor cutoffs
  incomplete in both general and Git-helper capture.
- Protect each just-persisted stdout/stderr trace pair from the pruning pass,
  count it toward both retention ceilings, and revalidate both files before
  publishing references so pruning can never create a dangling complete trace.
- Keep asynchronous command graphs on the raw output plane so capture modes do
  not reorder job bytes, retroactively apply `wait` redirections, or terminate a
  detached child when a one-shot parent exits; structured async observations
  remain explicitly deferred to a graph-wide durable collector.
- Hand opaque descendants' retained capture descriptors to detached bounded
  safety drains after the direct child exits, preventing late stdout/stderr from
  receiving `EPIPE`; helper failure waits for real EOF, and post-cutoff traces
  are explicitly incomplete rather than replayed or mislabeled exact.
- Treat non-TTY `rich` execution as raw before spawning a command, so large
  redirected or piped streams bypass render buffers and preserve bytes/status.
- Accept `&` before compound closers and honor escaped quotes in balanced
  process-substitution, array, extglob, and arithmetic scanners.
- Emit aggregate and per-stream raw-trace status in every semantic JSON
  observation, including complete captures and JSON-table summaries.

### Release engineering
- Pin every GitHub Action to an immutable commit, run CI and release gates with
  least-privilege tokens, and use the repository's exact Rust toolchain instead
  of a floating `stable` channel.
- Prepare archives that include the optional deep-interception library, sign and
  notarize all three macOS Mach-O files, smoke-test the hardened helper boundary,
  and validate release tags against Cargo and changelog versions before publishing.
- Harden `install.sh` with strict version/checksum parsing, optional immutable-
  release attestation verification, an exact archive allowlist, and symlink
  rejection; prepare the installer itself for checksummed publication.
- Add lockfile-fresh third-party license notices, rtk Apache-2.0 attribution,
  weekly Cargo/Actions Dependabot updates, and explicit non-publishable Cargo
  package metadata, including the workspace minimum Rust version, for the
  GitHub-release-only workspace.
- Prepare checksummed AGPL Corresponding Source with the exact tagged project
  tree and locked vendored Rust dependencies; rebuild it with kernel-denied
  network access, require immutable stable release tags, and revalidate the
  annotated tag immediately before publication.
- Support the initial immutable private release on GitHub Free with exact
  checksums, its automatic GitHub release-integrity attestation, and Apple
  Developer ID signing/notarization. Actions build-provenance attestations are
  explicitly unavailable for private repositories on this plan.
- Exercise offline installer packaging independently for Linux and Darwin in
  both CI and the release gate, including exact target archive, `.so`/`.dylib`,
  and platform-license membership.
- Exercise macOS loader hardening with a real system arm64e executable and a
  supported x86_64/Rosetta handoff, avoiding non-platform preview-ABI arm64e
  libraries that macOS 15 may reject before process startup; keep the generated
  caller marker newline-accurate and report the exact failed smoke assertion.

### Session resilience and developer experience

The resilience milestone: agsh now separates the three lifetimes every other
shell welds together — the terminal, the shell state, and the processes.
Kept processes survive closed windows and dropped SSH; session journals can
restore retained state after crashes and reboots. It also
ships a richer native history workflow for browsing, filtering, and reusing
commands without giving up shell-native storage.

### Added — session resilience
- **The keep broker** (`agshd`, new `agsh-broker` crate) — a per-user daemon
  that owns pseudo-terminals, so kept processes belong to it rather than to the
  window that started them. One PTY per job; output journaled to rotating logs
  plus a scrollback ring replayed on attach; jobs get a real controlling
  terminal (Ctrl-C works) via a safe-Rust `setsid`+`TIOCSCTTY` supervisor shim.
  Auto-started on first use; socket and state are 0700/0600.
- **`keep` builtin** — `keep -- CMD` runs a command that survives the terminal;
  `keep list / attach / tail / kill / rm / stop`. On a TTY it attaches
  immediately (Ctrl-] detaches); without one (agents, scripts) it spawns
  detached and reports id + hints as a captured observation.
- **Full-session keep** — `agsh --keep` runs the whole interactive session
  under the broker: closing the terminal or losing SSH only *detaches* it;
  `agsh --attach [ID]` resumes it exactly where it was, scrollback replayed.
  Plain interactive startup shows a breadcrumb when detached sessions exist.
- **Session journal + `resume`** — interactive sessions append state deltas
  (cwd, exports, vars, aliases, abbreviations, functions, set/shopt options,
  running jobs) to a bounded append-only per-session JSONL journal as they happen —
  crash-only design, nothing is "saved at exit". `resume` / `resume list` /
  `resume N` replay a dead session's deltas onto a new shell (never re-running
  commands). Background jobs that survived are rediscovered with a
  pid-reuse-safe liveness check; a `claude`/`codex` agent that died with the
  session gets its true resume path (`sessions`). A retained confinement record
  replays narrow-only; journaling itself is best effort and not a security boundary.
- **Wake-from-standby detection** — after sleep, agsh prints
  "system was asleep ~2h — 1 background job still running" instead of silently
  pretending time didn't pass.
- **`[session]` config** — `restore_banner = true` (or `AGSH_RESUME_BANNER=1`)
  opts into a startup banner for dead sessions that likely lost work. Off by
  default: a hangup at an idle prompt (how most people close windows) never
  interrupts anyone.
- **Release workflow** — prepares prebuilt binaries (macOS arm64/x86_64, Linux
  x86_64/aarch64 musl), checksums, and `install.sh`. Tagged macOS builds require
  Developer ID signing and notarization secrets and fail closed when absent.
  The isolated workflow is the sole publisher of release artifacts.

### Added — history and command entry
- **Native rich `history` workbench** — the history UI is scrollable over all
  matches instead of a fixed short page, with search-first navigation and
  metadata that includes the recorded date and time for each command.
- **Date-aware history filtering** — `history --today`, `history --date
  YYYY-MM-DD`, `history --since/--after YYYY-MM-DD`, and
  `history --before YYYY-MM-DD` filter by recorded command time using clear day
  boundaries.
- **Syntax-highlighted history commands** — TTY history views color commands
  with the same shell-aware highlighter used by the editor, gated so pipes,
  redirects, and captured output remain plain bytes.
- **Faster completion selection** — the completion menu numbers visible rows and
  supports direct Alt/Meta number selection, so common picks do not require
  repeated arrow navigation.
- **Environment-aware completion** — `export NAME=` can complete values from
  command history and the current environment, while `export NAME` and
  `unset NAME` complete variable names in the form each builtin expects.

### Added — earlier unreleased work
- **Startup rc file** — interactive sessions source `~/.config/agsh/agshrc`
  (aliases, functions, exports, prompt hooks, `mode:…`); `--norc` /
  `--rcfile` / `$AGSH_RC`.
- **Shell interception** (opt-in via `AGSH_INTERCEPT`) — route an agent's own
  `bash -c …` through agsh so its output is compacted/observed instead of
  bypassing it. Proxy (default, runs the real shell), native-interpret
  (`:native`), and a deep exec-interposition layer (`:deep`) that also catches
  absolute-path `/bin/bash` and `posix_spawn` via
  `DYLD_INSERT_LIBRARIES`/`LD_PRELOAD`. The deep layer is the isolated
  `agsh-intercept` crate, one of two executable-boundary `unsafe` exceptions.
- **`sessions`** now shows each session's folder; namespaced `mode:<aspect>`
  builtin.

### Changed
- **Compact mode never anti-compacts.** A successful command whose whole output
  is at most 3 short lines is shown without headline/count scaffolding or
  home/workspace path shortening (which used to render `compact pwd` as a lone
  `.`). Observation-only ANSI stripping and secret redaction still apply;
  semantic mode and user `[[compactor]]` rules are unaffected.
- **`raw:` references are now useful.** Emitted only when the compact view
  actually elides output (no more redundant pointer under fully-shown results),
  and under interception (`$AGSH_TRACE_DIR`) they are **catable file paths**
  backed by on-disk persistence — so an agent can `grep`/`cat` the full raw
  output from plain bash.
- **Inline autosuggestions are display-clipped** to the current line — a
  pathological multi-thousand-character history entry hints with `…` instead of
  flooding the screen; accepting with `→` still inserts the full command.
- The generic compact reducer credits its baseline: ported and extended from
  [rtk](https://github.com/rtk-ai/rtk), natively integrated into the shell.
- The prompt now emits OSC 7 current-directory integration so compatible
  terminals track `cd` changes live.
- Release gates now run the full behavioral suite before publishing artifacts;
  private `v0.2.0` assets ship with checksums and an immutable-release
  attestation, while Actions build-provenance attestations remain unavailable on
  the current private Free-plan repository.

### Fixed
- **PTY controller fd leak** — spawned jobs inherited their own PTY controller
  (rustix, unlike std, does not set CLOEXEC), so broker shutdown could never
  hang them up, silently orphaning shells. Also fixed in the `pty` builtin.
- **Attach handshake/install race** — the daemon confirmed an attach before
  installing it, so two back-to-back attaches could invert on a slow machine
  and hang up the newer client. Handshake, takeover, replay, and install are
  now one atomic section under the output lock.
- **Honest takeover reporting** — a client whose attach is taken over now says
  "taken over by another client — keeps running" instead of falsely reporting
  the job exited.

### Known limitations (first release of the new subsystems)
- The keep broker is new. Reattach is byte-replay, not screen reconstruction —
  full-screen TUIs may be momentarily garbled until they repaint on the resize
  signal. One attached client per job (last attach wins). A frozen attached
  client is detached after a bounded write deadline rather than stalling the job.
- `resume` does not journal arrays/associative arrays yet.
- `confine` is kernel-enforced on macOS only; elsewhere it fails closed
  (Linux Landlock planned).

## [0.1.0] - 2026-06-30 development milestone

Initial private-development baseline of the Aegis Shell — a from-scratch,
POSIX-inspired pre-1.0 shell written in Rust for both humans and AI coding agents.

### Shell language & execution
- POSIX-style grammar: pipelines, lists (`;`, `&&`, `||`, `&`), compound commands
  (`( )`, `{ }`, `if`/`for`/`while`/`until`/`case`), functions, and newlines —
  differential-tested against `bash` (198/200, 2 documented divergences) and `sh`
  (43/43).
- Expansions: parameter (`${x:-…}`, `${#x}`, `${x/a/b}`, …), command substitution,
  arithmetic (`$(( ))`), brace, tilde, globbing, and word splitting.
- Builtins: `cd`, `pwd`, `export`, `unset`, `echo`, `printf`, `read`, `test`/`[`/`[[`,
  `eval`, `source`, `alias`, `trap`, `set`, `local`, background jobs, partial
  `fg`/`bg`, and more; complete terminal job control is not claimed.
- Redirections, here-docs/here-strings, and exact byte-for-byte pipe semantics.

### For AI agents
- **Token-economy output modes** (`--output` / `AGSH_OUTPUT_MODE`): `raw`, `compact`,
  `semantic`, `lossless-ref`, `silent`, `rich`. Agents receive compact structured
  observations; successfully persisted `complete` raw bytes are referenced by
  `trace://`, while truncation, failure, and expiry are explicit.
- **`confine` capability sandbox** — kernel-enforced (macOS `sandbox-exec`)
  restriction of filesystem, network, and exec for a leaf payload. Composable
  presets — `read-only` (deny writes/network/common credential paths, private scratch),
  `workspace` (writes only within `$PWD`), `offline` (network off) — plus the v1
  exec-allowlist (`confine ls,df -- ./script`). Closes the interpreter-bypass gap
  where a confined `python evil.py` could still delete files. Self-managing agents
  (claude, …) are refused; fails closed when no backend is available. `confine`
  currently requires macOS; Linux (Landlock) support is planned and fails closed
  until then.

### Agent workflow
- **`sessions`** — discover the Claude Code and Codex sessions that ran in the
  current folder (and subfolders) and resume one (`sessions N` → `claude --resume`
  / `codex resume`), matched by the real cwd recorded in each session; each row is
  a clickable transcript link in hyperlink-aware terminals.

### Interactive experience
- Native line editor: syntax highlighting, completion dropdown, history with
  reverse search and autosuggestions, multi-line editing, and shell integration.
- Themed UI (truecolor/256/16-color aware) with `precmd`/`preexec`/`chpwd` hooks.
- Colorized `ls` (files vs directories) by seeding the real tool's color env on a
  TTY — never reimplementing `ls`, never corrupting piped output.
- **`agview FILE`** rich rendering: markdown, JSON, CSV/TSV, diffs, and now:
  - **Inline images** — crisp via the iTerm2/Kitty graphics protocols, with a
    universal truecolor half-block fallback so images display in any color terminal.
  - **Syntax highlighting** for Python, Rust, JavaScript, TypeScript, Go, C, C++,
    Java, Ruby, Shell, SQL, Lua, and config formats.

### Quality
- `unsafe` is forbidden across ordinary first-party crates. Executable-boundary
  operations are isolated in the optional `agsh-intercept` FFI library and the
  one-call `agsh-signal` SIGPIPE reset wrapper.
- Regression tests cover 2,000-deep adversarial parser/executor inputs, and
  security decisions remain deterministic.
- Hardened via a multi-agent audit: recursion-depth guards make deeply nested
  `$(( ))` / `math` expressions error cleanly instead of overflowing the stack;
  large stdin to a captured command no longer deadlocks; `view` of a binary on a
  pipe/redirect emits exact bytes; a pipeline whose consumer exits early
  (`… | head`) is silent (SIGPIPE) like bash; the history log streams on load and
  compacts so it can't grow without bound, with atomic appends.
- CI on Linux and macOS: `cargo fmt`/`clippy -D warnings`/`build`/`test` plus the
  golden, differential (bash + sh), and interactive (PTY) suites.
