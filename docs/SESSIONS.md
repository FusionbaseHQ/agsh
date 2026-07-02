# Session resilience

Every other shell welds three lifetimes together: the terminal, the shell's
state, and the child processes. Close the laptop lid on a dropped SSH
connection, quit the terminal app, or hit a crash, and all three die at once —
your working directory, exports, aliases, and any running agent session are
simply gone.

`agsh` separates them. Interactive sessions journal their state as it changes,
so the *state* survives the shell; the journal doubles as a flight recorder, so
a new session knows what was *running* when the old one died and how to bring
it back.

## How it works

Each interactive session appends its state **deltas** to a per-session JSONL
journal the moment they are observed (diffed at every command boundary):

- working directory
- exported variables and shell-local variables (`unset` included)
- aliases, abbreviations, functions
- `set` options (`errexit`, `pipefail`, …) and `shopt` flags
- background jobs (`job` / `job_end`, with pgid and registration time)
- the foreground command line (`fg` / `fg_end` — the flight recorder)

This is **crash-only design**: there is no save-on-exit step to miss. A clean
exit appends an `exit` record; a journal *without* one, whose shell pid is
gone, marks a session that died — crash, closed terminal (recorded as `hup`),
or reboot. At most the command in flight is lost.

Journaling is strictly interactive (TTY-gated). `agsh -c`, scripts, and piped
sessions never journal, so non-interactive behavior is byte-identical.

## `resume` — restore a dead session

At interactive startup, a muted one-line banner appears when something is
restorable:

```
agsh: a session hung up 12m ago (cwd ~/dev/api, 3 changes, `claude` was running) — `resume` restores it
```

```sh
resume          # restore the most recent dead session
resume list     # show restorable sessions (age, cwd, changes, what ran)
resume N        # restore the Nth listed one
```

Restore **replays the folded deltas** onto the live shell — last write wins per
key; commands are never re-run, so restoring has no side effects. Afterwards:

- the foreground command that died gets a program-aware hint — Claude/Codex
  sessions are genuinely resumable (`sessions` finds the transcript and
  `sessions N` reattaches the agent with its context); other commands get a
  rerun suggestion.
- background jobs are probed: a job whose process is alive *and* whose start
  time matches the journal (guarding against pid reuse) is reported as a
  survivor with a signal hint; the rest are reported as lost.

A restored journal is marked consumed (`restored` record) so it is never
offered twice. The next command boundary re-journals the applied state into
the *new* session's journal, so restore chains across repeated crashes.

## Wake-from-standby detection

Sleep doesn't kill processes — it freezes them and drops TCP connections. On
wake, agsh notices the divergence between the wall clock and the monotonic
clock and says what happened instead of silently pretending time didn't pass:

```
agsh: system was asleep ~2h — 1 background job still running
```

## Storage and security

Journals live in `$AGSH_SESSION_DIR`, else `$XDG_STATE_HOME/agsh/sessions`,
else `~/.local/state/agsh/sessions` — one `<id>.jsonl` per session, directory
`0700`, files `0600` (they can carry exported secrets), capped at 40 files
(oldest pruned at startup). Corrupt lines are skipped, never truncating the
newer events after them. `$AGSH_SESSION` carries the session id into child
processes.

Two security properties hold across restore:

- **Confinement cannot widen.** `AGSH_CONFINE` replays through the narrow-only
  `set_confine` path, so a session that died confined comes back confined.
- **Session identity is never replayed.** `AGSH_SESSION`, `PWD`/`OLDPWD`,
  `SHLVL`, and positional parameters are excluded from journaling and replay.

## Boundaries (what this does not do)

Restore brings back *state*, not *processes*: a foreground process that died
with its terminal is not resurrected — it is reported, with a real resume path
when one exists (Claude/Codex today). Keeping processes themselves alive
across terminal death requires a PTY broker that owns their lifetimes — that
is the planned next layer, deliberately separate from the journal.
