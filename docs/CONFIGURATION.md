# Configuration

`agsh` reads its configuration from `~/.config/agsh/`. Everything has a sensible
default; nothing is required. Example files live in [`configs/agsh/`](../configs/agsh).

## Files

| File                        | Purpose                                             |
| --------------------------- | --------------------------------------------------- |
| `~/.config/agsh/token.toml` | output modes and token-economy defaults             |
| `~/.config/agsh/config.toml`| general shell settings                              |
| `~/.config/agsh/policies/`  | `confine`/allowlist policy files                    |
| `~/.config/agsh/agshrc`     | **startup rc** — sourced at interactive startup (aliases, functions, prompt, `export`s, `mode:…`) |

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

Copy an example to get started:

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

The config / `mode` default applies to **interactive** sessions only —
non-interactive `agsh -c` and scripts stay `raw`, so piped output is never
silently transformed.

| Mode           | Use                                                     |
| -------------- | ------------------------------------------------------- |
| `raw`          | default; exact bytes, streamed                          |
| `compact`      | trimmed, deduplicated output                            |
| `semantic`     | structured observation of recognized commands           |
| `lossless-ref` | compact view + a `trace://` reference to the raw stream |
| `silent`       | suppress display, keep exit status + trace              |
| `rich`         | human rich rendering (see `agview`)                     |

### The `mode` builtin

```sh
mode:output compact   # set the output aspect (mode compact is a shorthand)
mode:output           # show one aspect
mode                  # show all aspects
mode:output off       # reset to the startup default
```

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
- `AGSH_INTERCEPT=compact:deep` — also catch **absolute-path** shell calls
  (`/bin/bash -c …`) and `posix_spawn` (what node/libuv use), via an injected
  interposition library (`DYLD_INSERT_LIBRARIES` / `LD_PRELOAD`).

**Recovering raw output.** Compacted results only carry a `raw:` reference when they
actually elide output. Under interception those references are **catable file paths**
(agsh persists each observed command's raw stdout/stderr to `$AGSH_TRACE_DIR`), so an
agent can pull back exactly what it needs from plain bash:

```sh
# a compacted result ends with, e.g.:
#   raw: /…/agsh-traces/1234_cmd_….out /…/agsh-traces/1234_cmd_….err
grep -n "error" /…/agsh-traces/1234_cmd_….out     # query the full raw output
```

The trace directory is **bounded** — on every write the oldest files are reaped so
it never grows without limit (default 512 files ≈ 256 commands; override with
`AGSH_TRACE_DIR_CAP`).

> **Coverage.** Without `:deep`, interception catches shells resolved by name or via
> `$SHELL` (a program calling `/bin/bash` by absolute path bypasses it). `:deep`
> closes that gap, but is best-effort: macOS **SIP / hardened-runtime** binaries
> strip `DYLD_INSERT_LIBRARIES`, and `LD_PRELOAD` is ignored by **static** binaries
> and across setuid execs — those fall back to the (still active) PATH shims.

Set it in your `agshrc` so it applies to every session, or per-agent:
`AGSH_INTERCEPT=compact agsh -c 'my-agent …'`.

`AGSH_INTERCEPT` is read once at startup. To toggle interception **within** a running
interactive session (affects newly launched commands), use the `mode` builtin:

```sh
mode:intercept compact:deep   # turn on now
mode:intercept                # show on/off
mode:intercept off            # turn off
```

## Environment variables

| Variable            | Effect                                                    |
| ------------------- | -------------------------------------------------------- |
| `AGSH_OUTPUT_MODE`  | default output mode for the session                       |
| `AGSH_INTERCEPT`    | route the agent's `bash`/`sh`/… through agsh (mode name; off by default) |
| `AGSH_ICONS=1`      | enable Nerd Font glyphs in the UI                         |
| `NO_COLOR`          | disable color (honored)                                   |

## Themes

Colors adapt to the terminal's detected capability (truecolor / 256 / 16). Palette
and role mapping are configurable; see [`THEMING`](../configs/agsh) examples and the
`agsh-style` crate for the role vocabulary.
