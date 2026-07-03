# Changelog

All notable changes to `agsh` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-03

The resilience release: agsh now separates the three lifetimes every other
shell welds together — the terminal, the shell state, and the processes.
Sessions survive closed windows, dropped SSH, crashes, and reboots.

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
  running jobs) to a crash-safe per-session JSONL journal as they happen —
  crash-only design, nothing is "saved at exit". `resume` / `resume list` /
  `resume N` replay a dead session's deltas onto a new shell (never re-running
  commands). Background jobs that survived are rediscovered with a
  pid-reuse-safe liveness check; a `claude`/`codex` agent that died with the
  session gets its true resume path (`sessions`). Confinement replays
  narrow-only, so a confined session can't come back widened.
- **Wake-from-standby detection** — after sleep, agsh prints
  "system was asleep ~2h — 1 background job still running" instead of silently
  pretending time didn't pass.
- **`[session]` config** — `restore_banner = true` (or `AGSH_RESUME_BANNER=1`)
  opts into a startup banner for dead sessions that likely lost work. Off by
  default: a hangup at an idle prompt (how most people close windows) never
  interrupts anyone.
- **Release channel** — prebuilt binaries (macOS arm64/x86_64, Linux
  x86_64/aarch64 musl) with checksums on GitHub Releases, and a hosted
  `install.sh` with platform detection + sha256 verification. The macOS
  binaries are signed (Developer ID, hardened runtime) and notarized by the
  release pipeline; tagged builds fail if signing secrets are missing, so an
  unsigned release can't slip out. See `docs/RELEASING.md`.

### Added — earlier unreleased work
- **Startup rc file** — interactive sessions source `~/.config/agsh/agshrc`
  (aliases, functions, exports, prompt hooks, `mode:…`); `--norc` /
  `--rcfile` / `$AGSH_RC`.
- **Shell interception** (opt-in via `AGSH_INTERCEPT`) — route an agent's own
  `bash -c …` through agsh so its output is compacted/observed instead of
  bypassing it. Proxy (default, runs the real shell), native-interpret
  (`:native`), and a deep exec-interposition layer (`:deep`) that also catches
  absolute-path `/bin/bash` and `posix_spawn` via
  `DYLD_INSERT_LIBRARIES`/`LD_PRELOAD`. The deep layer is the new, isolated
  `agsh-intercept` crate — the single first-party `unsafe` exception.
- **`sessions`** now shows each session's folder; namespaced `mode:<aspect>`
  builtin.

### Changed
- **Compact mode never anti-compacts.** A successful command whose whole output
  is at most 3 short lines is shown verbatim — no headline/counts scaffolding,
  and no home/workspace path shortening (which used to render `compact pwd` as
  a lone `.`). ANSI-stripping and secret redaction still apply; semantic mode
  and user `[[compactor]]` rules are unaffected.
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
  client can stall its job's output pump until it detaches; hardening planned.
- `resume` does not journal arrays/associative arrays yet.
- `confine` is kernel-enforced on macOS only; elsewhere it fails closed
  (Linux Landlock planned).

## [0.1.0] - 2026-06-30

First production-ready release of the Aegis Shell — a from-scratch, POSIX-style
shell written in Rust for both humans and AI coding agents.

### Shell language & execution
- POSIX-style grammar: pipelines, lists (`;`, `&&`, `||`, `&`), compound commands
  (`( )`, `{ }`, `if`/`for`/`while`/`until`/`case`), functions, and newlines —
  differential-tested against `bash` (198/200, 2 documented divergences) and `sh`
  (43/43).
- Expansions: parameter (`${x:-…}`, `${#x}`, `${x/a/b}`, …), command substitution,
  arithmetic (`$(( ))`), brace, tilde, globbing, and word splitting.
- Builtins: `cd`, `pwd`, `export`, `unset`, `echo`, `printf`, `read`, `test`/`[`/`[[`,
  `eval`, `source`, `alias`, `trap`, `set`, `local`, job control, and more.
- Redirections, here-docs/here-strings, and exact byte-for-byte pipe semantics.

### For AI agents
- **Token-economy output modes** (`--output` / `AGSH_OUTPUT_MODE`): `raw`, `compact`,
  `semantic`, `lossless-ref`, `silent`, `rich`. Agents receive compact structured
  observations; raw bytes remain recoverable via `trace://` references.
- **`confine` capability sandbox** — kernel-enforced (macOS `sandbox-exec`)
  restriction of filesystem, network, and exec for a leaf payload. Composable
  presets — `read-only` (no writes/network/secret-reads, private scratch),
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
- `unsafe` is forbidden across all first-party crates.
- 0 panics across a 2000-input adversarial fuzz; deterministic security.
- Hardened via a multi-agent audit: recursion-depth guards make deeply nested
  `$(( ))` / `math` expressions error cleanly instead of overflowing the stack;
  large stdin to a captured command no longer deadlocks; `view` of a binary on a
  pipe/redirect emits exact bytes; a pipeline whose consumer exits early
  (`… | head`) is silent (SIGPIPE) like bash; the history log streams on load and
  compacts so it can't grow without bound, with atomic appends.
- CI on Linux and macOS: `cargo fmt`/`clippy -D warnings`/`build`/`test` plus the
  golden, differential (bash + sh), and interactive (PTY) suites.
