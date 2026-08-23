# Architecture

`agsh` is a Cargo workspace of focused crates. Each has a single responsibility and
a small public surface; the binary crate wires them together.

```text
crates/
  agsh/          CLI binary, raw-exec helper binary, interactive entry point
  agsh-core/     lexer, parser, command-graph IR, values, shell errors
  agsh-exec/     shell state, builtins, executor, expansion, confine sandbox
  agsh-policy/   capabilities, risk analyzer, command allowlist
  agsh-output/   output modes, compaction, token-economy observations
  agsh-signal/   isolated SIGPIPE reset for raw-exec intermediary paths
  agsh-render/   rich rendering: markdown, JSON, CSV, code, images
  agsh-style/    theme, palette, color levels, roles
  agsh-tty/      line editor, completion, history, syntax highlighting
  agsh-agent/    bounded agent-protocol codec + session path model (server planned)
  agsh-broker/   keep broker: PTY-owning daemon, protocol, attach client
  agsh-store/    trace, history, and session-journal store
  agsh-index/    project / filesystem indexer
  agsh-compat/   command resolution / POSIX compatibility
```

## Execution pipeline

1. **Lex + parse** (`agsh-core`): source → tokens → a command-graph IR covering
   pipelines, lists (`;`, `&&`, `||`, `&`), compound commands (`( )`, `{ }`,
   `if`/`for`/`while`/`until`/`case`), functions, redirections, and here-docs.
2. **Expand** (`agsh-exec`): parameter, command, arithmetic, brace, tilde, glob,
   and word splitting.
3. **Resolve** (`agsh-compat`): builtin vs external; external commands run normally.
4. **Execute** (`agsh-exec`): processes, pipes, and redirections with exact
   byte-for-byte stream semantics; optional `confine` capability sandbox.
5. **Observe** (`agsh-output`): in a non-raw output mode, the raw stream is captured
   and rendered into a compact observation. When persistence succeeds, a
   `complete` private backing path contains exact bytes within the configured
   cap; live sessions additionally address indexed streams as `trace://`.
   Persistence failure, expiry, truncation, and disabled storage are explicit;
   `agtrace` reads are themselves bounded.

## Design contract

- Developers type normal commands; external commands execute normally.
- Environment variables, pipes, and redirects behave normally and receive **exact
  bytes**. Observation modes are opt-in for noninteractive use; automatic rich
  rendering is additionally TTY-gated.
- Supported foreground captures give agents compact structured observations;
  parsed asynchronous graphs currently use the documented raw fallback.
  Retained raw output is status-addressable and never mislabeled exact after
  storage truncation.
- The shell never silently rewrites standard commands (`ls`, `git`, `python`, …)
  into custom alternatives.

## Safety properties

- `unsafe` is **forbidden** throughout the shell and every ordinary first-party
  crate (`unsafe_code = "forbid"`). Two executable-boundary exceptions are
  isolated from shell logic: the optional `agsh-intercept` preload library needs
  `execve`/`posix_spawn` FFI, and `agsh-signal` wraps one audited `SIGPIPE`
  disposition reset for raw-exec intermediary entry paths. The interposer is
  opt-in; the shell reaches the signal operation only through a narrow safe API.
- Regression tests cover deeply nested parser/executor inputs that previously
  risked stack exhaustion; security behavior is deterministic.
- Kernel-backed `confine` presets are fail-closed: without a supported backend
  they refuse. The explicitly requested `--best-effort` shim and sticky
  allowlist are guardrails, not security boundaries (see
  [`CONFINE.md`](CONFINE.md)).

## Testing

- Rust unit/integration tests across all crates.
- Golden output checks, differential parity vs `bash` and `sh`, and interactive
  (PTY) editor/completion/render suites under `tests/`.
- CI runs formatting, `clippy -D warnings`, build, and all suites on Linux and
  macOS.
