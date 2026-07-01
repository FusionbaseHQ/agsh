# Changelog

All notable changes to `agsh` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
