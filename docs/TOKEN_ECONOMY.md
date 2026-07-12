# Token-economy output layer

The output layer provides compact agent-facing observations without changing
Unix stream semantics. Its primary rule is non-negotiable:

```text
pipes, redirects, files, and child processes receive exact raw bytes
normalization, redaction, rendering, and compaction affect observations only
```

## Output planes

1. The process stream is the child's exact stdout/stderr and feeds Unix pipes
   and redirections.
2. The human display is raw by default. `rich` rendering is opt-in and TTY
   gated.
3. A captured observation can normalize and compact a bounded preview while the
   raw stream is teed to private trace files up to the configured storage cap.

The trace directory is mode `0700` and trace files are mode `0600`. They are
unredacted and readable by other processes with the same user authority.

## Modes

| Mode | Current behavior |
| --- | --- |
| `raw` | Stream exact output; do not capture for observation rendering |
| `clean` | Normalize ANSI/progress noise and redact the observation |
| `compact` | Apply family/generic reduction within the configured budget |
| `semantic` | Emit a structured summary with status, failures, counts, and raw refs |
| `lossless-ref` | Emit a small observation plus raw references; explicitly report when storage truncation prevents lossless recovery |
| `silent` | Suppress display while preserving status and trace references when persistence succeeds |
| `rich` | TTY-oriented rendering for recognized human-facing content |

Selection priority is per-command wrapper, `--output`, the interactive `mode`
builtin, `AGSH_OUTPUT_MODE`, interactive `token.toml`, then `raw`. Noninteractive
`agsh -c`, script, and piped-input execution stays raw unless explicitly
overridden. There is no agent server yet, so `[mode].agent_default` does not
currently select a protocol-session default.

A top-level graph containing a parsed asynchronous-list `&` currently forces
`raw` for the whole graph. Capturing and replaying each job at a later `wait`
would change launch-time descriptor routing and output chronology, while an
in-process collector would die before an un-waited child of a one-shot shell.
Until a graph-wide ordered collector can outlive that parent, raw fallback is
the only supported behavior; `silent`, `compact`, `semantic`, and
`lossless-ref` are therefore unavailable for such graphs.

## Capture, budgets, and retention

Captured stdout/stderr is spooled incrementally; only bounded head/tail previews
are retained for observation construction. Token estimates use approximately
four characters per token. Defaults are 2,000 tokens, an 8,000-token maximum,
and `lossless-ref` fallback.

`[storage].max_raw_per_command` limits the combined persisted stdout and stderr
for one top-level command (default `100mb`, binary units). All concurrent stream
spools share that budget, readers continue draining after it is exhausted, and
the child's exit status is unchanged. Values above the non-configurable 1 GiB
ceiling are clamped. A capped reference is explicitly marked as partial with
`raw_trace: incomplete`; semantic JSON always carries aggregate and per-stream
status, including successful complete persistence. Exact trace
APIs refuse incomplete data unless the caller opts into the status-aware
API. `store_raw = false` drains without storing raw bytes or creating durable
trace files.

Trace persistence is best effort for ordinary observation modes: an unavailable
trace directory or a later write/sync failure marks the affected raw reference
`unavailable`, distinct from storage explicitly `disabled` by configuration,
but does not replace the child's exit status or captured observation bytes. With
enabled storage, explicitly selected `lossless-ref` mode validates
the private trace directory before starting the command; an unsafe or
unavailable directory therefore fails before the payload runs. A later failure
still leaves the child status intact and reports the reference unavailable.

Capture readers also stop waiting for pipe EOF once the direct child has exited
and no bytes are immediately available, so a detached descendant that retains
stdout/stderr cannot stall command completion. At that cutoff, the read end is
offered to a fixed-buffer drain worker in its own process group with `/` as its
working directory. The capture reader relinquishes ownership only after the
worker flushes a readiness acknowledgement; failed or timed-out setup kills and
reaps the worker group before local draining resumes, and ambiguous setup marks
the trace incomplete. A single reaper thread tracks acknowledged workers while
the shell remains alive. The worker discards later bytes until all inherited
writers close, preventing an
opaque descendant from receiving `EPIPE` after the one-shot parent exits; it
does not replay or reorder those bytes. Because kernel EOF was not observed by
the capturing process, such a cutoff is marked incomplete even when every byte
seen before handoff was stored. If the trusted helper cannot be launched, the
reader keeps draining to real EOF rather than closing the pipe and changing the
descendant's behavior; command completion may therefore wait in that degraded
case. Nested, non-spooled compound output has a hard
64 MiB aggregate memory ceiling and fails explicitly rather than growing an
unbounded buffer; disk-backed exact segments retain bounded in-memory previews.

These helpers are safety drains, not structured asynchronous collectors. Each
worker owns exactly one retained stream and discards only bytes produced after
capture cutoff. A command whose descendants retain both stdout and stderr can
therefore leave two fixed-buffer workers alive until kernel EOF, potentially for
the descendant's full lifetime. This process-per-retained-stream cost is a
documented pre-1.0 limitation. There is no global admission bound yet, so
repeated commands with indefinitely retained descriptors can create an
unbounded aggregate number of live workers. A session-wide supervised drain
service should eventually replace them. This does not make compact or semantic
capture generally available for parsed asynchronous graphs, which still use
the raw fallback.

Recovery is intentionally finite. The in-session trace index retains 200
commands. The persisted directory defaults to 512 files (roughly 256 commands)
and also has a hard 2 GiB aggregate ceiling; oldest files are pruned when either
bound is exceeded. `AGSH_TRACE_DIR_CAP` overrides the count but is clamped to
4,096 and cannot bypass the byte ceiling. A raw reference can therefore expire
and must not be treated as permanent storage.

A relative `AGSH_TRACE_DIR` is anchored to the shell's startup directory, and
persisted references are emitted as absolute paths. Changing the shell cwd does
not retarget preflight, persistence, or an already-issued reference.

`agtrace` opens retained streams through the status-aware streaming reader.
Full output is capped at 16 MiB; line selections are capped at 5,000 lines,
16 MiB total, and 1 MiB per input line. Unnumbered selections preserve the
selected line bytes, including CRLF and non-UTF-8 data. Grep summaries show at
most 100 matches and scan at most 1 GiB.
Truncated, unavailable, and disabled streams are reported distinctly with
status 2, never as a missing reference. The legacy allocating resolver is also
capped at 16 MiB and marks a larger returned prefix as truncated.

The `[storage].raw_retention` duration is reserved and is not yet enforced; file
count pruning remains the active age/retention mechanism.

## Reduction pipeline

`compact` and `semantic` observations are normalized, deterministically
redacted, classified from parsed argv, passed to a configured or native family
compactor when available, otherwise passed through the generic reducer, and
finally budgeted. The compact tiny-output path still normalizes and redacts but
skips summary scaffolding. `clean` only normalizes/redacts (falling back to a
reference when over budget), while `lossless-ref`, `silent`, and `rich` use their
dedicated paths rather than the compactor pipeline. The generic reducer
collapses progress/blank/duplicate lines, removes known noise, clips long lines,
and keeps a head/tail window. A non-empty input has a non-empty fallback.

Built-in family compactors cover common Git, compiler/test, search, package,
Docker/Kubernetes, and build-tool output. Low-priority presets in
`crates/agsh-output/src/presets.toml` cover additional commands. User TOML rules
win by priority and can use `match_output`, replacements, strip/keep patterns,
line truncation, head/tail windows, and `on_empty` messages.

## Security properties and limits

- Observation redaction is deterministic and occurs after raw capture. It does
  not alter pipes or persisted trace bytes.
- Redaction covers configured literal secret values/names and bounded token-like
  patterns; it is not data-loss prevention or exhaustive secret discovery.
- Command-family detection is deterministic, not an LLM classification.
- Compactors must preserve the real exit code and must never suppress a guarded
  error into a success message.
- Regex/rule compilation and application are bounded, but family summaries are
  still heuristics and must retain raw references when output is elided.

The reference config is [`configs/agsh/token.toml`](../configs/agsh/token.toml).
Implemented parser/schema types live in `agsh-output::config`; fields described
as reserved above must not be presented as active policy.
