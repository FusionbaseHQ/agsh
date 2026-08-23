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
or reboot. Records are appended in bounded single writes so a partial final
record does not prevent the loader from continuing at later complete lines.
Journaling is best effort: oversized events, a full journal, lock contention,
and I/O errors drop a record without failing the shell. Appends are not
synchronously flushed at every command boundary, so abrupt power loss can also
lose recent deltas that the OS had not yet made durable.

Journaling is strictly interactive (TTY-gated). `agsh -c`, scripts, and piped
sessions never journal, so non-interactive behavior is byte-identical.

## `resume` — restore a dead session

An optional muted one-line startup banner can point at a dead session that
likely *lost work*:

```
agsh: a session hung up 12m ago (cwd ~/dev/api, 3 changes, `claude` was running) — `resume` restores it
```

The banner is **off by default** — even a good heuristic interrupts people who
end sessions by closing windows all day. Opt in via `token.toml`:

```toml
[session]
restore_banner = true
```

(or `AGSH_RESUME_BANNER=1`, which also overrides the config; `=0` force-quiets
it.) Even when enabled it is conservative: it fires only for a crash (no `hup`
record — SIGKILL, panic, power loss) or a hangup/termination while something
was still running, and only for deaths in the last 48 hours. A hangup at an
idle prompt is how most people close a terminal window — those sessions never
banner. Banner or no banner, retained records remain discoverable:

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
  survivor with a signal hint; mismatches are reported as lost, while a
  transient or permission-denied probe is reported as unverifiable.

After replay, agsh best-effort appends a `restored` marker. A successfully
persisted marker prevents the journal from being offered twice; write failure
can leave it eligible again. Resume is therefore not transactional. The next
command boundary similarly attempts to journal the applied state in the new
session.

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
(oldest pruned at startup). Journal paths are opened without following their
final symlink and only regular files are accepted, so FIFOs and devices cannot
block shell startup or command-boundary writes. An encoded event is capped at
1 MiB, a load or append inspects at most 64 MiB per journal, and at most 16,384
decoded events are retained in memory. Oversized or corrupt lines are skipped,
never truncating newer valid events after them. `$AGSH_SESSION` carries the
session id into child processes.

Two replay properties apply to records that are present:

- **A recorded confinement cannot widen during replay.** `AGSH_CONFINE` uses
  the narrow-only `set_confine` path. A missing/non-durable record can omit
  confinement entirely, so `resume` is not a security boundary; re-establish
  confinement explicitly before running untrusted work.
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

The layers compose: a kept session still journals its state deltas, so if the
*broker* dies (for example on reboot), `resume` can restore records that reached
durable storage. Terminal death keeps the broker-owned process alive; host
death leaves only the durable journal state to restore.

### Broker storage & security

Broker state lives in `$AGSH_BROKER_DIR` (else `$XDG_STATE_HOME/agsh/broker`,
else `~/.local/state/agsh/broker`): a 0700 directory holding the 0600 control
socket (`agshd.sock` — the ssh-agent model: only the owning user can reach it),
per-job logs (rotated at 8 MiB, ≤2 generations), and the daemon log. The socket
is private from bind time and the daemon verifies the peer UID of every accepted
connection. An `AGSH_BROKER_SOCKET` override is accepted only when its existing
parent is a real directory owned by the current user with mode 0700; unsafe
shared parents such as `/tmp` fail closed rather than being chmodded.
Job environments are passed explicitly by the spawning shell, so confinement
(`AGSH_CONFINE`) propagates into kept jobs like any child. Stopping the broker
hangs up its jobs (their PTYs close) — that is the documented `keep stop`
contract, never an accident.

An already running broker is not hot-upgraded when a new agsh binary is
installed. After an upgrade, finish or deliberately stop kept jobs, run
`keep stop`, and let the next `keep` command start the matching broker. Do not
use `keep stop` as an automatic installer hook: stopping the broker hangs up all
of its jobs.

Broker clients are same-UID peers, not separately authenticated principals.
Control traffic is capped at 64 concurrent connections and 4 MiB per JSON line,
with 5-second control I/O deadlines; one tail response is capped at 16 MiB.
Logs and job environments are unredacted and may contain secrets. Running jobs
have a hard daemon-wide ceiling of 64. Each job log rotates at approximately
8 MiB with one old generation; only the newest 20 finished records remain, and
pruning or a successful `keep rm` deletes both log generations. Pruning keeps
the record count bounded even if an unlink fails, reports the cleanup error in
the daemon log or request response, and leaves the startup orphan sweep a later
cleanup attempt. A daemon generation holds a private advisory lock, so startup
cleanup remains safe even if a live daemon's socket pathname was manually
removed. After acquiring that lock, startup retains at most the newest 20 orphan
job IDs and 128 MiB from the
prior generation; exact-name symlink entries are unlinked without following
them and an unexpected non-regular log fails startup. The daemon log rotates at
1 MiB with one old generation; the serialized accept loop is its runtime
rotation/reopen checkpoint, so one bounded diagnostic may overshoot the
threshold before the next connection. Recognized job-output storage is thus
bounded by the 20-ID / 128 MiB startup window plus at most 64 running and 20
finished jobs, each with two approximately 8 MiB generations. There is no
time-based expiry. Logging and cleanup are best effort and are not fsynced for
every write. A job-log
write/rotation failure disables further disk logging for that job rather than
discarding the byte ceiling; PTY draining and bounded in-memory scrollback keep
running.

## Boundaries (what this does not do)

`resume` replays state, never processes; `keep` preserves processes, never
retroactively — a command you didn't `keep` (or a session not under `--keep`)
still dies with its terminal, and the journal's flight recorder + resume
recipes are the fallback. The broker holds one attached client per job (last
attach wins). Any broker exit closes its controller PTYs and normally delivers
SIGHUP; a command that ignores SIGHUP may survive but is unreachable. Reattach is
byte-replay, not screen reconstruction: a full-screen TUI may look garbled for
a moment until it repaints on the resize signal. Writes to an attached client
have a 250 ms deadline: if a client freezes or stops consuming output, the
broker drops that attachment and continues draining the PTY into its bounded
scrollback and rotating log. The process keeps running and can be reattached;
a persistently slow client may therefore be detached rather than allowed to
stall the job. Client input blocked by a full PTY also has a 250 ms deadline;
the attachment is dropped and the error is recorded in the daemon log instead
of holding the controller lock forever. Once the direct child has been reaped,
new attaches, input, resize, and signal requests fail deterministically rather
than targeting a potentially reused PID or process group.

Attach EOF is resolved with a token-scoped terminal-status response. This status
survives immediate pruning of the ordinary finished record; up to 64 unclaimed
attach statuses are retained, after which the oldest expires and its client gets
an explicit status error instead of a guessed exit code.

The broker currently represents `cwd` as a JSON string. A requested UTF-8 cwd
must exist and be an openable directory or spawning fails explicitly; it never
falls back to the daemon's cwd. Non-UTF-8 working-directory bytes are not yet
representable by this protocol version, even though opaque environment entries
are preserved byte-for-byte.
