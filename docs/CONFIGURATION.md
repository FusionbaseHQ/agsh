# Configuration

In 0.2, `agsh` has two active file-based configuration surfaces: output/session
settings in `token.toml`, and interactive shell commands in `agshrc`. Everything
has a sensible default; neither file is required. Examples and future design
references live in [`configs/agsh/`](../configs/agsh).

## Files

| File | 0.2 status | Purpose |
| --- | --- | --- |
| `~/.config/agsh/token.toml` | Active | output, trace-storage, session-banner, normalization, redaction, and compactor settings |
| `~/.config/agsh/agshrc` | Active | interactive startup commands: aliases, functions, hooks, exports, and `mode:…` |
| `~/.config/agsh/config.toml` | Design reference only | proposed general shell settings; **not loaded** |
| `~/.config/agsh/policies/` | Design reference only | proposed policy files; **not loaded** |

The last two example paths are intentionally checked in for design discussion,
not as accepted runtime input. Built-in `confine` presets and explicit command
flags are the current enforcement interface. Unknown files under the config
directory have no effect.

### Startup rc file

Interactive sessions source a startup rc file — the place for your aliases,
functions, exports, prompt hooks (`precmd`/`preexec`/`chpwd`), and a default mode.
The first file found is used:

1. `--rcfile PATH` or `$AGSH_RC`
2. `~/.config/agsh/agshrc` (recommended)
3. `~/.agshrc` (dotfile fallback)

Start from the template ([`agshrc.example`](../configs/agsh/agshrc.example)):

```sh
mkdir -p ~/.config/agsh
cp configs/agsh/agshrc.example ~/.config/agsh/agshrc
```

Only **interactive** sessions source it — `agsh -c …`, scripts, and piped input do
not, so scripted behavior is never affected. Skip it with `--norc` (or
`AGSH_NORC=1`). A syntax error in the rc is reported but never blocks startup.

Copy the active token configuration example to get started:

```sh
mkdir -p ~/.config/agsh
cp configs/agsh/token.toml ~/.config/agsh/token.toml
```

## Output modes

Every command can be rendered in a mode. Priority, highest first:

1. per-command wrapper — `semantic git diff`
2. `--output` flag — `agsh --output compact -c 'pytest -q'`
3. `mode` builtin (session default) — `mode:output compact`
4. `AGSH_OUTPUT_MODE` environment variable
5. `~/.config/agsh/token.toml` `[mode] default`
6. built-in default — `raw`

```toml
# ~/.config/agsh/token.toml
[mode]
default = "compact"   # applies to interactive sessions only
```

The token config / `mode` default applies to **interactive** sessions only.
Non-interactive `agsh -c` and scripts stay `raw` unless `--output` or
`AGSH_OUTPUT_MODE` explicitly selects an observation mode, so automation is not
silently transformed by interactive configuration.

| Mode           | Use                                                     |
| -------------- | ------------------------------------------------------- |
| `raw`          | default; exact bytes, streamed                          |
| `clean`        | normalized observation without ANSI/progress noise      |
| `compact`      | trimmed, deduplicated output                            |
| `semantic`     | structured observation of recognized commands           |
| `lossless-ref` | compact view + status-aware ref; exact only when persistence is `complete` |
| `silent`       | suppress display; keep status and a trace when persistence succeeds |
| `rich`         | human rich rendering (see `agview`)                     |

### The `mode` builtin

```sh
mode:output compact   # set the output aspect (mode compact is a shorthand)
mode:output           # show one aspect
mode                  # show all aspects
mode:output off       # reset to the startup default
```

## Session resilience (`[session]`)

```toml
# ~/.config/agsh/token.toml
[session]
restore_banner = true   # startup banner for dead sessions with lost work
                        # (off by default; AGSH_RESUME_BANNER=1|0 overrides)
```

`resume`, `resume list`, and the keep broker need no configuration; see
[SESSIONS.md](SESSIONS.md).

## Shell interception (for agents)

Coding agents often run commands as their own `bash -c '…'` subprocess, which
executes *outside* agsh and so bypasses its output modes. Opt in to **interception**
to route those shell calls back through agsh, which runs the real shell and renders
its output in the chosen mode:

```sh
export AGSH_INTERCEPT=compact    # off by default; also accepts semantic, etc.
```

When enabled, agsh installs `bash`/`sh`/`zsh`/… shims (on `PATH` + `$SHELL`) that
forward to `agsh --observe`, which runs the **real** shell and captures its output —
so semantics are exact and only the observed output is compacted. Nested shells pass
straight through (no double-observation), and raw pipes still receive exact bytes.

Flavors and layers (combine with `:`):

- `AGSH_INTERCEPT=compact` — **proxy** (default): runs the real shell, observes it.
  Exact semantics; recommended.
- `AGSH_INTERCEPT=compact:native` — **interpret**: agsh runs the command in its own
  interpreter (full agsh features, but bounded by agsh's `bash` compatibility).
- `AGSH_INTERCEPT=compact:deep` — experimentally catch some **absolute-path** shell calls
  (`/bin/bash -c …`) and `posix_spawn` (what node/libuv use), via an injected
  interposition library (`DYLD_INSERT_LIBRARIES` / `LD_PRELOAD`).

**Recovering raw output.** Compacted results only carry a `raw:` reference when they
actually elide output. Under interception those references are **catable file paths**
(agsh persists each observed command's raw stdout/stderr to `$AGSH_TRACE_DIR`), so an
agent can pull back exactly what it needs from plain bash while the trace status
is `complete`:

```sh
# a compacted result ends with, e.g.:
#   raw: /…/agsh-traces/1234_cmd_….out /…/agsh-traces/1234_cmd_….err
grep -n "error" /…/agsh-traces/1234_cmd_….out     # query retained raw output
```

Raw persistence is configured in `~/.config/agsh/token.toml`:

```toml
[storage]
store_raw = true
max_raw_per_command = "100mb" # combined stdout+stderr; b/kb/mb/gb are binary
raw_retention = "14d"         # reserved; duration pruning is not implemented
```

The per-command value is clamped to a hard 1 GiB ceiling. Capture continues
draining after the shared stdout/stderr budget is exhausted, but the reference
is labeled as an incomplete capture and exact trace reads refuse it. Setting
`store_raw = false` stores no raw bytes. The directory is independently bounded
to 2 GiB and, by default, 512 files (roughly 256 commands); oldest files are
reaped after each write when either bound is exceeded. `AGSH_TRACE_DIR_CAP`
overrides the count but is clamped to 4,096 and cannot bypass the byte ceiling.
Old references can expire.

Relative `AGSH_TRACE_DIR` values are anchored to the shell's startup directory;
persisted references are absolute and do not move after `cd`. Trace persistence
is best effort in ordinary observation modes. Enabled `lossless-ref` storage is
validated before the payload starts, while a later directory, write, or sync
failure marks the reference unavailable without replacing the child's status.
A capture stopped because a descendant retained a pipe descriptor is marked
incomplete rather than exact. `agtrace` returns at most 16 MiB and 5,000 selected
lines per invocation (1 MiB per input line), and grep scans at most the hard
1 GiB trace ceiling. Use ordinary streaming tools directly on a known
`complete` raw file when a larger view is required.

> **Experimental coverage, not confinement.** Without `:deep`, interception catches
> shells resolved by name or via `$SHELL` (a program calling `/bin/bash` by absolute
> path bypasses it). `:deep` is a best-effort observation aid, not comprehensive exec
> mediation or a security boundary. It hooks a limited exec/posix_spawn surface;
> macOS **SIP / hardened-runtime** binaries strip `DYLD_INSERT_LIBRARIES`, and
> `LD_PRELOAD` is ignored by **static** binaries and across setuid execs. Preload
> hooks also cannot guarantee async-signal-safety in every post-fork, multi-threaded
> child. Unsupported calls may still fall back to the active PATH shims, but policy
> enforcement must use `confine`, never interception coverage. Prebuilt Linux
> archives ship a glibc interposer beside the otherwise static-musl `agsh` binary;
> musl-only systems retain PATH-shim interception but cannot load that `.so`.
>
> Executable text entered through agsh's explicit ENOEXEC `/bin/sh` fallback is
> kept inside one raw observation subtree. `AGSH_INTERCEPT_ACTIVE=1` prevents
> `:deep` re-entry throughout that subtree so pipes and redirects retain exact
> bytes. agsh removes only its own preload-library entry before starting the
> fallback interpreter, preserving unrelated caller preloads and avoiding a
> loader-architecture failure in Apple system shells. An absolute shell launched
> from that fallback is therefore not observed again. This is a documented
> experimental interception gap, not a security boundary.

On macOS, names beginning `AGSH_INTERNAL_EXEC_DYLD_V1_` are reserved for the
private hardened-helper environment transport. agsh removes caller-provided
bindings in that namespace before external targets start; applications should
not use the prefix.

Set it in your `agshrc` so it applies to every session, or per-agent:
`AGSH_INTERCEPT=compact agsh -c 'my-agent …'`.

`AGSH_INTERCEPT` is read once at startup. To toggle interception **within** a running
interactive session (affects newly launched commands), use the `mode` builtin:

```sh
mode:intercept compact:deep   # turn on now
mode:intercept                # show on/off
mode:intercept off            # turn off
```

## Resource limits

agsh rejects oversized control/input files before parsing them. These limits
bound memory use and prevent device/FIFO-backed configuration from blocking
startup:

| Input | Limit |
| --- | ---: |
| shell script, piped script, or `source` file | 64 MiB |
| interactive rc file | 1 MiB |
| `token.toml` | 1 MiB |
| `theme.toml` | 256 KiB |
| trusted project `.env` | 1 MiB |
| agent `peek`/`patch` target | 64 MiB |
| agent `patch` diff | 16 MiB |
| persisted history JSONL record | 1 MiB |
| command retained in history | 256 KiB |
| persistent history file | 64 MiB |
| aggregate in-memory history | 32 MiB |
| in-memory exact capture (command substitution, rich/internal evaluation) | 64 MiB per stream |
| process-substitution staging | shared `max_raw_per_command`; 64 MiB per stream when raw storage is disabled |
| nested compound capture / ordering metadata | 64 MiB / 1,048,576 spans / 65,536 exact segments |
| one `agtrace` result / selected lines / input line | 16 MiB / 5,000 / 1 MiB |
| Git subprocess used by `snapshot` | 4 MiB per stream / 30 seconds |
| one `read` logical input line | 1 MiB |
| one builtin `printf` outcome | 16 MiB |
| one `pty` captured stream | 64 MiB |
| asynchronous-subshell state handoff | 8 MiB / 16,384 entries per collection / 65,536 total entries |
| one session-journal record / file / decoded event count | 1 MiB / 64 MiB / 16,384 |
| broker JSON control line / active connections / control I/O | 4 MiB / 64 / 5 seconds |
| broker tail response / per-job log generations | 16 MiB / two approximately 8 MiB files |
| broker running jobs / retained finished records | 64 / 20 |
| prior-generation broker logs retained at startup | 20 job IDs and 128 MiB |
| daemon log rotation | 1 MiB, checked on accept / one old generation |
| active captured streams / detached capture-drain helpers per shell | 64 shared admissions / at most 64 processes |

External commands, pipes, and redirections still stream raw bytes without
passing through these buffers. The in-memory capture limit applies only where
shell semantics require the complete output as a value; exceeding it is an
explicit execution error instead of an unbounded allocation.

Broker running/retained-job limits are daemon-wide. Job-log cleanup is tied to
record pruning/removal and a generation-locked startup sweep. Retention has no
time-based expiry; see [`SESSIONS.md`](SESSIONS.md) for daemon-log overshoot and
the same-UID trust boundary.

## Environment variables

| Variable            | Effect                                                    |
| ------------------- | -------------------------------------------------------- |
| `AGSH_OUTPUT_MODE`  | default output mode for the session                       |
| `AGSH_INTERCEPT`    | route the agent's `bash`/`sh`/… through agsh (mode name; off by default) |
| `AGSH_TRUST_FILE`   | override the private project-`.env` trust database path   |
| `AGSH_ICONS=1`      | enable Nerd Font glyphs in the UI                         |
| `NO_COLOR`          | disable color (honored)                                   |

`agtrust` persists a versioned SHA-256 digest before activating a project
`.env`. Legacy unversioned trust records are intentionally ignored and must be
re-trusted. Leaving the project restores each affected shell binding's original
value, export state, and variable attributes.

## Themes

Colors adapt to the terminal's detected capability (truecolor / 256 / 16). Palette
and role mapping are configurable; see [`THEMING`](../configs/agsh) examples and the
`agsh-style` crate for the role vocabulary.
