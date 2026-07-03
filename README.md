<div align="center">

# `agsh` — Aegis Shell

**The shell that never loses your work — and speaks fluent agent.**

[![CI](https://github.com/FusionbaseHQ/agsh/actions/workflows/ci.yml/badge.svg)](https://github.com/FusionbaseHQ/agsh/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-8b949e.svg)](#install-in-60-seconds)
[![Rust](https://img.shields.io/badge/built%20with-Rust%2C%20no%20unsafe-f74c00.svg)](#-safe-by-construction)

A modern POSIX-style shell, written from scratch in Rust, for humans **and** AI
coding agents. Your commands run unchanged, your sessions survive closed
laptops, and your agents burn fewer tokens.

</div>

---

```console
~/api ❯ agsh --keep
agsh: kept session [k1] — closing this terminal only detaches it

~/api ❯ claude        # agent three hours into a refactor…

  # …lid closes. SSH drops. Terminal app quits. Doesn't matter.

  # later, in any new terminal:
~ ❯ agsh --attach
  # → back inside k1: claude still running, scrollback replayed,
  #   cwd, env, and children exactly where you left them
```

## Why agsh?

Every other shell welds three lifetimes together: the **terminal**, the
**shell state**, and the **processes**. Close the window and all three die.
`agsh` separates them — and adds the observation layer agents have been
missing.

### 🔌 Your work survives the terminal

```sh
keep -- npm run dev   # close the terminal — the dev server keeps running
keep list             # any later shell sees it
keep attach k1        # reattach, scrollback replayed (Ctrl-] detaches)

agsh --keep           # or keep the WHOLE session
agsh --attach         # terminal death only detaches; this resumes it
```

A per-user PTY broker (`agshd`, auto-started) owns kept processes, so they
belong to *it*, not to the window you happened to start them in. Real
controlling terminal — Ctrl-C works — output journaled to disk while nobody
watches. Lifetime and scrollback **without tmux's windows, panes, and
prefix-key world**.

### 🧯 …and even survives crashes and reboots

Interactive sessions journal their state *as it changes* — crash-only design,
nothing is "saved on exit". After a crash, kill, or reboot:

```sh
resume            # cwd, exports, aliases, functions, options — restored
resume list       # every restorable session (age, cwd, what was running)
```

Restore replays state deltas; it never re-runs commands. If a `claude`/`codex`
agent died with the session, agsh points at its true resume path. And after
standby, agsh says *"system was asleep ~2h — 1 background job still running"*
instead of pretending time didn't pass.

### 🤖 Built for AI agents

Agents don't need 3,000 lines of `cargo test` noise in their context window —
they need what failed, and a pointer to the rest:

```sh
semantic git diff                        # per-command
agsh --output compact -c 'pytest -q'     # per-invocation
mode:output compact                      # session default, changeable live
```

```jsonc
// what an agent sees instead of a wall of raw output:
{
  "command": "cargo test",
  "exit_code": 1,
  "status": "failed",
  "headline": "exit 1: 2 failure-like line(s)",
  "failures": ["test keep::attach ... FAILED", "..."],
  "raw_stdout": "trace://cmd_42/stdout"      // full bytes, on demand
}
```

Six modes (`raw` · `clean` · `compact` · `semantic` · `lossless-ref` ·
`silent`), family-aware compactors for git/cargo/test-runners/docker/…, and
every raw byte stays recoverable via `trace://` references. Small outputs pass
through verbatim — compaction never costs you information.

The generic line reducer is ported and extended from the excellent
[**rtk** (Rust Token Killer)](https://github.com/rtk-ai/rtk) — but **natively
integrated into the shell** instead of a proxy you remember to prefix: it
applies to every command in any capturing mode, composes with the family
compactors and `trace://` recovery, and stays configurable through the same
`[[compactor]]` TOML rules.

### 🛡️ A sandbox you can actually enforce

```sh
confine read-only  -- python analyze.py   # read+run; no writes, network, or secret reads
confine workspace  -- ./build.sh          # writes only inside $PWD (+ scratch)
confine offline    -- npm test            # network off
confine convert    -- ./thumb.sh          # may exec ONLY `convert`
```

Kernel-enforced (macOS Seatbelt; Linux Landlock planned — **fails closed**, it
never runs a payload it can't restrict). No LLM judgment calls, no prompt-level
"please don't" — a hard floor *beneath* an agent's own permission system.

### ⚡ …and it's still just a shell

Pipelines, lists, functions, here-docs, redirections, the full expansion set —
differential-tested against `bash` (198/200) and `sh` (43/43) on every commit.
`agsh` **never** silently rewrites `ls`, `git`, or `python` into custom
alternatives, and pipes/redirects always receive exact bytes. Plus a fast
themed editor: syntax highlighting as you type, completion dropdown, inline
autosuggestions, reverse search.

## What that looks like day to day

| You want…                                    | Elsewhere                    | In agsh                      |
| -------------------------------------------- | ---------------------------- | ---------------------------- |
| Dev server survives the closed laptop        | tmux/screen ceremony         | `keep -- npm run dev`        |
| Session survives dropped SSH                 | tmux + config                | `agsh --keep` → `--attach`   |
| cwd/env/aliases back after a crash or reboot | gone                         | `resume`                     |
| Agent reads a test run                       | full raw dump in context     | `semantic` summary + `trace://` |
| Run an untrusted script safely               | hope                         | `confine read-only -- ./it`  |
| Jump back into yesterday's Claude session    | hunt for the terminal        | `sessions` → `sessions 2`    |

## Install in 60 seconds

**Prebuilt binary** (macOS arm64/x86_64, Linux x86_64/arm64 — static musl;
verifies checksums, no sudo, installs to `~/.local/bin`):

```sh
curl -fsSL https://raw.githubusercontent.com/FusionbaseHQ/agsh/main/install.sh | sh
```

Prefer to read before you run? `curl -fsSLO …/install.sh && less install.sh &&
sh install.sh`. Pin a version with `AGSH_VERSION=v0.2.0`, change the target dir
with `AGSH_INSTALL_DIR`.

**From source** (stable [Rust toolchain](https://rustup.rs)):

```sh
git clone https://github.com/FusionbaseHQ/agsh.git && cd agsh
cargo build --release
install -m755 target/release/agsh ~/.local/bin/agsh   # or anywhere on PATH
agsh
```

Then take it for a spin:

```sh
keep -- python3 -m http.server   # now close this terminal. open a new one:
keep list                        # …still running
keep attach k1                   # welcome back (Ctrl-] detaches)

agview README.md                 # rendered markdown, in your terminal
agview photo.png                 # inline images (iTerm2/Kitty/WezTerm/Ghostty,
                                 #   truecolor half-blocks everywhere else)
semantic git status              # what your agent would see
sessions                         # Claude/Codex sessions in this folder — resumable
```

Non-interactive use works everywhere immediately: `agsh -c 'echo hello'`.

## The finer print (worth knowing)

<details>
<summary><b>Output modes — selection & precedence</b></summary>

Priority, highest first: per-command wrapper (`semantic git diff`) →
`--output` flag → `mode` builtin → `AGSH_OUTPUT_MODE` env →
`~/.config/agsh/token.toml`:

```toml
[mode]
default = "compact"   # interactive sessions only
```

| Mode           | Use                                                     |
| -------------- | ------------------------------------------------------- |
| `raw`          | default; exact bytes, streamed                          |
| `clean`        | raw minus ANSI/control noise                            |
| `compact`      | trimmed, deduplicated; tiny outputs pass through as-is  |
| `semantic`     | structured JSON observation of recognized commands      |
| `lossless-ref` | compact view + `trace://` reference to the raw stream   |
| `silent`       | suppress display, keep exit status + trace              |
| `rich`         | human rich rendering (see `agview`)                     |

Session defaults apply to **interactive** sessions only — `agsh -c`, scripts,
and pipes stay `raw`, so automation is never silently transformed. Secrets are
redacted from observations by default (`[security]` in the config).

</details>

<details>
<summary><b>Session resilience — the full model</b></summary>

Three independent layers (see [docs/SESSIONS.md](docs/SESSIONS.md)):

1. **State journal** — every interactive session appends cwd/env/alias/function
   deltas to a crash-safe JSONL journal; `resume` replays them. An optional
   startup banner (`[session] restore_banner = true`, off by default) points at
   sessions that likely lost work.
2. **Keep broker** — `agshd` owns PTYs for kept jobs and sessions; terminals
   only ever *detach*. Output is logged (rotated) + a scrollback ring replays
   on attach. Last attach wins; `keep stop` shuts the broker down (and hangs up
   its jobs — documented, deliberate).
3. **Resume recipes** — what can't be kept alive (host death) gets recognized:
   Claude/Codex sessions resurface via `sessions N` with their context intact.

The layers compose: a kept session still journals, so even broker/host death
degrades to `resume`, not to nothing.

</details>

<details>
<summary><b>Naming — why <code>agview</code> and not <code>view</code></b></summary>

agsh's own tools are `ag`-prefixed *only where a bare name would shadow a real
CLI* (`agview` vs vim's `view`, `agpatch` vs `patch`, `agmath`, `agz`,
`agjump`, `agtrust`, `agcontext`, `agtrace`). Conflict-free tools keep the
clean bare name (`confine`, `peek`, `risk`, `snapshot`, `pty`, `keep`,
`resume`, `sessions`) and also accept an `ag…` alias. Your muscle memory for
real tools always wins.

</details>

<details>
<summary><b>Repository layout</b></summary>

```text
crates/
  agsh/          CLI binary and interactive shell entry point
  agsh-broker/   keep broker: PTY-owning daemon, protocol, attach client
  agsh-core/     lexer, parser, command graph IR, values, shell errors
  agsh-exec/     shell state, builtins, executor, expansion, confine
  agsh-policy/   capabilities, risk analyzer, command allowlist
  agsh-output/   output modes, compaction, token-economy observations
  agsh-render/   rich rendering: markdown, json, csv, code, images
  agsh-style/    theme, palette, color levels, roles
  agsh-tty/      line editor, completion, history, highlighting
  agsh-agent/    agent protocol/server
  agsh-store/    trace, history, and session-journal store
  agsh-index/    project/filesystem indexer
  agsh-compat/   command resolution / POSIX compatibility

docs/            architecture, confine sandbox, configuration, sessions
tests/           golden checks, differential (vs bash/sh), interactive (PTY)
```

</details>

<details>
<summary><b>Development & test suites</b></summary>

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

CI runs all of the above on Linux and macOS
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

</details>

## 🦀 Safe by construction

`unsafe` is **forbidden** in every first-party crate — including the PTY
broker daemon (the optional `agsh-intercept` preload shim is the single,
isolated exception, and it's never linked into the shell). The parser and
executor are fuzzed for panic-freedom, and security behavior is deterministic
by design: no LLM ever makes a sandboxing decision.

## The promises

1. **Normal commands run normally.** No silent rewrites, ever.
2. **Pipes and redirects receive exact bytes.** Rich rendering and compaction
   are display-only and TTY-gated.
3. **Agents get structure; raw stays recoverable.** Every observation can point
   back to the exact bytes.
4. **Security is enforced, not suggested.** Kernel sandboxing that fails
   closed, deterministic risk analysis.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — crates, execution pipeline, design contract
- [Session resilience](docs/SESSIONS.md) — journaling, `resume`, the keep broker, wake detection
- [`confine` sandbox](docs/CONFINE.md) — capability sandbox, presets, guarantees
- [Configuration](docs/CONFIGURATION.md) — output modes, config files, environment

## License

Copyright © 2026 Fusionbase and the `agsh` contributors.

Licensed under the **GNU Affero General Public License v3.0** — see
[`LICENSE`](LICENSE). The AGPL's network-use clause (section 13) applies: if
you run a modified version of `agsh` and let users interact with it over a
network, you must offer them the corresponding source. Contributions are
accepted under the same license.
