# Aegis Shell (`agsh`)

A modern, from-scratch POSIX-style shell written in Rust — built for both humans
and AI coding agents. It runs your normal Unix commands unchanged, adds a fast
themed interactive editor, and gives agents a structured, token-efficient view of
the world plus a kernel-enforced command guardrail.

`agsh` never silently rewrites `ls`, `git`, `python`, `cargo`, or any other
command into a custom alternative, and pipes/redirects always receive exact bytes.
Native accelerations and rich displays are always opt-in (`agview file.py`,
`semantic git diff`).

```sh
agsh --output semantic -c 'cargo test'      # compact, structured agent view
agview src/main.rs                           # syntax-highlighted source
agview diagram.png                           # inline image (any terminal)
confine ls,df -- ./monitor.sh                # kernel-confined to ls + df (macOS)
```

## Highlights

- **POSIX-compatible** — pipelines, lists (`;`, `&&`, `||`, `&`), compound
  commands, functions, here-docs, redirections, and the full expansion set
  (parameter, command, arithmetic, brace, tilde, glob). Differential-tested
  against `bash` (198/200) and `sh` (43/43).
- **Built for agents** — token-economy output modes (`raw`, `compact`,
  `semantic`, `lossless-ref`, `silent`, `rich`) deliver compact structured
  observations while keeping raw output recoverable via `trace://` references.
- **`confine` capability sandbox** — kernel-enforced (macOS) restriction of the
  filesystem, network, and which commands a payload may run: `confine read-only --
  python x.py`, `confine workspace -- ./build.sh`, `confine offline -- npm test`.
  A hard floor *beneath* an agent's own permissions.
- **Rich `agview`** — markdown, JSON, CSV/TSV, diffs, **inline images** (iTerm2/Kitty
  protocols with a universal truecolor half-block fallback), and **syntax
  highlighting** for a dozen languages.
- **Modern interactive editor** — syntax highlighting, completion, history with
  reverse search and autosuggestions, themed truecolor UI, and `precmd`/`preexec`/
  `chpwd` hooks.
- **Safe by construction** — `unsafe` is forbidden in all first-party crates, the
  shell is fuzzed for panic-freedom, and security behavior is deterministic.

## Install

Requires a stable Rust toolchain (see `rust-toolchain.toml`).

```sh
git clone https://github.com/FusionbaseHQ/agsh.git && cd agsh
cargo build --release
# the binary is at target/release/agsh
install -m755 target/release/agsh ~/.local/bin/agsh   # or anywhere on PATH
```

Run it as your interactive shell:

```sh
agsh
```

…or run a single command and exit:

```sh
agsh -c 'echo hello'
```

## Usage

### Output modes (for agents and humans)

Select per command, per flag, or via the environment:

```sh
semantic git diff                       # per-command wrapper (one command)
agsh --output compact -c 'pytest -q'    # flag (whole invocation)
AGSH_OUTPUT_MODE=semantic agsh -c …     # env (whole session)
mode:output compact                     # set the session default at runtime
```

**Session default mode.** Make *every* command render in a mode — so `ls` behaves
like `compact ls` — by setting a session default. Priority (highest first):
per-command wrapper → `--output` → `mode` builtin → `AGSH_OUTPUT_MODE` →
`~/.config/agsh/token.toml`. In the config:

```toml
[mode]
default = "compact"   # applies to interactive sessions
```

Change it live with the `mode` builtin — a namespaced family so more mode aspects
can be added over time:

```sh
mode:output compact   # set the output aspect (mode compact is a shorthand)
mode:output           # show one aspect
mode                  # show all aspects
mode:output off       # reset to the startup default
```

The config/`mode` default applies to **interactive** sessions only — non-interactive
`agsh -c` and scripts stay `raw`, so piped output is never silently transformed.

| Mode           | Use                                                       |
| -------------- | --------------------------------------------------------- |
| `raw`          | default; exact bytes, streamed                            |
| `compact`      | trimmed, deduplicated output                              |
| `semantic`     | structured observation of recognized commands             |
| `lossless-ref` | compact view + a `trace://` reference to the raw stream   |
| `silent`       | suppress display, keep exit status + trace                |
| `rich`         | human rich rendering (see `agview`)                       |

### `agview` — rich rendering

```sh
agview README.md        # markdown
agview data.json        # pretty JSON
agview report.csv       # aligned table
agview change.diff      # colored diff
agview src/main.py      # syntax-highlighted code (py, rs, js, ts, go, c, …)
agview photo.jpg        # inline image (crisp in iTerm2/Kitty/WezTerm/Ghostty,
                        # truecolor half-blocks elsewhere)
```

Rich rendering is human-display only and TTY-gated: piped or redirected output
always remains the raw bytes.

> **Naming.** agsh's own tools are `ag`-prefixed *only where a bare name would
> shadow a common CLI* (`agview` vs vim's `view`, `agpatch` vs `patch`, `agmath`,
> `agz`, `agjump`, `agtrust`, `agcontext`, `agtrace`). The bare names are left to
> the real tools. Conflict-free tools keep the clean bare name (`confine`, `peek`,
> `risk`, `snapshot`, `pty`), and each also has an `ag…` alias for consistency.

### `confine` — capability sandbox for any tool or script

A composable, kernel-enforced sandbox (macOS Seatbelt). Restrict the filesystem,
network, and which commands a payload may run:

```sh
confine read-only -- python analyze.py   # read+run; no writes, no network, no secret reads
confine workspace -- ./build.sh          # writes only within the project ($PWD) + scratch
confine offline -- npm test              # network off; filesystem unchanged
confine convert,identify -- ./thumb.sh   # exec-allowlist: may only run convert/identify
confine read-only --rw ./out -- ./tool   # read-only except ./out is writable
confine --explain read-only -- foo       # print the granted/denied capabilities
```

`read-only`/`workspace` are no-network and secret-reads-denied by default (so a
script can't read `~/.ssh` *and* exfiltrate); a private scratch dir keeps tools
that need temp files working. The bare exec-allowlist form
(`confine ls,df -- monitor.sh`) and the launch form (`agsh --allow ls,df --run …`)
still work.

Self-managing agents (e.g. `claude`) are refused with guidance to use their own
permission systems — their broad runtime can't be reduced to a small allowlist.
`--force` overrides; `--best-effort` falls back to the shim layer. `confine`
currently requires **macOS** (Seatbelt); on Linux (Landlock is planned) and
elsewhere it **fails closed** rather than running unconfined. See
[`docs/CONFINE.md`](docs/CONFINE.md).

### `sessions` — resume Claude / Codex sessions

Find the Claude Code and Codex (OpenAI) sessions that ran in this folder (and its
subfolders) and jump back into one:

```sh
sessions          # list sessions here, newest first (agent, age, id, summary)
sessions 2        # resume the 2nd listed session
sessions --all    # every folder, not just this one
```

Resume runs `claude --resume <id>` / `codex resume <id>` from the session's
directory. In a hyperlink-aware terminal each row is clickable (opens the
transcript). Sessions are matched by the real `cwd` recorded inside each one.

## Repository layout

```text
crates/
  agsh/          CLI binary and interactive shell entry point
  agsh-core/     lexer, parser, command graph IR, values, shell errors
  agsh-exec/     shell state, builtins, executor, expansion, confine
  agsh-policy/   capabilities, risk analyzer, command allowlist
  agsh-output/   output modes, compaction, token-economy observations
  agsh-render/   rich rendering: markdown, json, csv, code, images
  agsh-style/    theme, palette, color levels, roles
  agsh-tty/      line editor, completion, history, highlighting
  agsh-agent/    agent protocol/server
  agsh-store/    trace and history store
  agsh-index/    project/filesystem indexer
  agsh-compat/   command resolution / POSIX compatibility

docs/            architecture, confine sandbox, configuration
tests/           golden checks, differential (vs bash/sh), interactive (PTY)
```

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — crates, execution pipeline, design contract
- [`confine` sandbox](docs/CONFINE.md) — capability sandbox, presets, guarantees
- [Configuration](docs/CONFIGURATION.md) — output modes, config files, environment

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Behavioral suites:
python3 tests/checks/run.py tests/checks/*.agsh   # golden output checks
python3 tests/differential/diff.py                # parity vs bash
python3 tests/differential/posix.py               # parity vs sh
python3 tests/interactive/run.py                  # PTY editor/completion/render
```

CI runs all of the above on Linux and macOS (`.github/workflows/ci.yml`).

## Design contract

- Developers type normal commands; external commands execute normally.
- Environment variables, pipes, and redirects behave normally and receive exact
  bytes.
- Agents receive compact structured observations; raw output stays recoverable.
- The shell never silently rewrites standard commands into custom alternatives.

## License

Copyright © 2026 Fusionbase and the `agsh` contributors.

Licensed under the **GNU Affero General Public License v3.0** — see [`LICENSE`](LICENSE).

The AGPL's network-use clause (section 13) applies: if you run a modified version of
`agsh` and let users interact with it over a network, you must offer them the
corresponding source. Contributions are accepted under the same license.
