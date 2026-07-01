# Architecture

`agsh` is a Cargo workspace of focused crates. Each has a single responsibility and
a small public surface; the binary crate wires them together.

```text
crates/
  agsh/          CLI binary + interactive shell entry point
  agsh-core/     lexer, parser, command-graph IR, values, shell errors
  agsh-exec/     shell state, builtins, executor, expansion, confine sandbox
  agsh-policy/   capabilities, risk analyzer, command allowlist
  agsh-output/   output modes, compaction, token-economy observations
  agsh-render/   rich rendering: markdown, JSON, CSV, code, images
  agsh-style/    theme, palette, color levels, roles
  agsh-tty/      line editor, completion, history, syntax highlighting
  agsh-agent/    agent protocol / server
  agsh-store/    trace and history store
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
   and rendered into a compact observation, with the raw bytes recoverable via a
   `trace://` reference.

## Design contract

- Developers type normal commands; external commands execute normally.
- Environment variables, pipes, and redirects behave normally and receive **exact
  bytes** — rich rendering and native accelerations are always opt-in and TTY-gated.
- Agents receive compact structured observations; raw output stays recoverable.
- The shell never silently rewrites standard commands (`ls`, `git`, `python`, …)
  into custom alternatives.

## Safety properties

- `unsafe` is **forbidden** in every first-party crate (`unsafe_code = "forbid"`).
- The parser/executor are fuzzed for panic-freedom; security behavior is
  deterministic.
- `confine` is fail-closed: it never runs a payload it cannot actually restrict
  (see [`CONFINE.md`](CONFINE.md)).

## Testing

- Rust unit/integration tests across all crates.
- Golden output checks, differential parity vs `bash` and `sh`, and interactive
  (PTY) editor/completion/render suites under `tests/`.
- CI runs formatting, `clippy -D warnings`, build, and all suites on Linux and
  macOS.
