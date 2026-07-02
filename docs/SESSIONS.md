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

At interactive startup, a muted one-line banner appears when a dead session
likely *lost work*:

```
agsh: a session hung up 12m ago (cwd ~/dev/api, 3 changes, `claude` was running) — `resume` restores it
```

The banner is deliberately conservative: it fires only for a crash (no `hup`
record — SIGKILL, panic, reboot) or a hangup while something was still running
(foreground command or background jobs), and only for deaths in the last 48
hours. A hangup at an idle prompt is how most people close a terminal window —
those sessions never banner, but stay quietly available:

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

## The keep broker: processes that survive the terminal

The journal restores *state*. Keeping *processes* alive across terminal death
is the broker's job: `agshd`, a per-user daemon that owns pseudo-terminals, so
a kept process's lifetime is tied to the daemon instead of the terminal that
started it. It is shpool-shaped, deliberately not tmux-shaped: one PTY per
job, no windows, no panes — just lifetime, logs, and scrollback.

### `keep` — keep one command

```sh
keep -- npm run dev     # start kept; on a terminal, attach immediately
keep list               # id, state, age, command (* = attached)
keep attach k1          # reattach with scrollback replay (Ctrl-] detaches)
keep tail k1            # last bytes of the output log
keep kill k1 [SIG]      # signal the job's process group
keep stop               # stop the broker (hangs up every kept job)
```

The broker auto-starts on first use. Jobs get a real controlling terminal
(`setsid` + `TIOCSCTTY` in the safe-Rust supervisor shim, so Ctrl-C works),
your exported env and cwd, a rotating on-disk output log, and a 64 KiB
scrollback ring replayed on attach. Detaching — by key or by your terminal
dying — never kills the job. Agents calling `keep` without a TTY get the id
and tail/attach hints as a captured observation.

### `agsh --keep` — keep the whole session

```sh
agsh --keep             # this interactive session now survives the terminal
agsh --attach [ID]      # reattach a detached session (newest, or by id)
```

Under `--keep`, the `agsh` you launched is a thin attach client; the real
session runs under the broker. Closing the window, killing the client, or
losing SSH during standby only *detaches* — the session keeps its cwd, env,
running children, everything. Plain interactive startup prints a breadcrumb
when detached sessions exist. Typing `exit` inside the session ends it for
real, and the client reports the exit code.

The layers compose: a kept session still journals its state deltas, so even if
the *broker* dies (reboot), `resume` restores the session's state on the next
start. Terminal death → broker keeps the process; host death → journal
restores the state.

### Broker storage & security

Broker state lives in `$AGSH_BROKER_DIR` (else `$XDG_STATE_HOME/agsh/broker`,
else `~/.local/state/agsh/broker`): a 0700 directory holding the 0600 control
socket (`agshd.sock` — the ssh-agent model: only the owning user can reach
it), per-job logs (rotated at 8 MiB, ≤2 generations), and the daemon log.
Job environments are passed explicitly by the spawning shell, so confinement
(`AGSH_CONFINE`) propagates into kept jobs like any child. Stopping the broker
hangs up its jobs (their PTYs close) — that is the documented `keep stop`
contract, never an accident.

## Boundaries (what this does not do)

`resume` replays state, never processes; `keep` preserves processes, never
retroactively — a command you didn't `keep` (or a session not under `--keep`)
still dies with its terminal, and the journal's flight recorder + resume
recipes are the fallback. The broker holds one attached client per job (last
attach wins), and killing the daemon degrades kept jobs to orphans (alive but
unreachable) — they are their own sessions, so they keep running.
