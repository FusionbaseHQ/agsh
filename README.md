<div align="center">

# `agsh` — Aegis Shell

**A resilient shell for persistent sessions and structured agent workflows.**

[![CI](https://github.com/FusionbaseHQ/agsh/actions/workflows/ci.yml/badge.svg)](https://github.com/FusionbaseHQ/agsh/actions/workflows/ci.yml)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-8b949e.svg)](#install-in-60-seconds)
[![Rust](https://img.shields.io/badge/built%20with-Rust%2C%20unsafe%20isolated-f74c00.svg)](#-safe-by-construction)

A modern POSIX-style shell, written from scratch in Rust, for humans **and** AI
coding agents. It is designed for ordinary command compatibility, kept sessions
that outlive terminal disconnects, and lower-token agent observations.

</div>

---

> **Release status: pre-1.0 preview.** The command executor, raw stream paths,
> output layer, and keep broker have broad macOS/Linux test coverage, but agsh is
> not yet a drop-in replacement for every POSIX/Bash script or a cross-platform
> security boundary. Full terminal job control, Linux kernel confinement, the
> authenticated agent server, and wider OS/terminal qualification remain open.
> See the [implementation status](docs/IMPLEMENTATION_PLAN.md) and
> [security model](docs/SECURITY_MODEL.md) before depending on it in production.

```console
~/api ❯ agsh --keep
agsh: kept session [k1] — closing this terminal only detaches it

~/api ❯ claude        # agent three hours into a refactor…

  # …lid closes. SSH drops. Terminal app quits. The client detaches.

  # later, in any new terminal:
~ ❯ agsh --attach
  # → back inside k1: the same process is running and recent byte scrollback
  #   is replayed (full-screen applications repaint after resize)
```

## Why agsh?

A conventional unbrokered shell often ties together three lifetimes: the
**terminal**, the **shell state**, and its **processes**. `agsh` can separate
them with an explicit keep broker, and adds a structured observation layer.

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
controlling terminal — Ctrl-C works — output logged to disk while nobody
watches. Lifetime and scrollback **without tmux's windows, panes, and
prefix-key world**.

### 🧯 Shell state can recover after crashes and reboots

Interactive sessions append restorable state *as it changes* — crash-only
design, nothing is "saved on exit". After shell termination, and after a reboot
for records the OS made durable:

```sh
resume            # replay available cwd/export/alias/function/option records
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
  "raw_stdout": "/private/.../agsh-traces-501/<pid>_cmd_42.out",
  "raw_trace": {
    "complete": true,
    "stdout": "complete",
    "stderr": "complete",
    "limit_bytes": 104857600
  }
}
```

Seven modes (`raw` · `clean` · `compact` · `semantic` · `lossless-ref` ·
`silent` · `rich`), family-aware compactors for git/cargo/test-runners/docker/…, and
successfully persisted `complete` traces remain exactly recoverable through the
private raw paths emitted in one-shot observations. In a live shell, `agtrace`
also addresses retained commands as `trace://<id>/<stream>`. Recovery lasts only
while the backing files remain retained **and** combined stdout/stderr stays within
`max_raw_per_command` (100 MiB by default). Output beyond that cap is still
drained, but the observation explicitly marks its stored prefix `truncated`;
`store_raw = false` marks recovery `disabled`, while an enabled storage path that
cannot be persisted is `unavailable`. The in-session index defaults to 200
commands; persisted output defaults to 512 files and is independently capped at
2 GiB. `agtrace` exposes bounded status-aware views (16 MiB per invocation);
references can also expire. Small compact outputs avoid summary scaffolding
after normalization and redaction. Observation rendering never changes bytes
delivered to pipes, redirects, or child processes.

Graphs that contain a parsed asynchronous-list `&` currently force `raw` mode
for the whole graph, even when another mode was requested. This preserves the
background job's launch-time descriptors, byte order, status, and lifetime; it
also means those graphs do not yet produce compact, semantic, silent, or
lossless-reference observations. Use explicit redirection or `agjob` when a
durable detached log is required. A graph-wide detached observation collector
is tracked as pre-1.0 work.

The generic line reducer is ported and extended from the excellent
[**rtk** (Rust Token Killer)](https://github.com/rtk-ai/rtk) — but **natively
integrated into the shell** instead of a proxy you remember to prefix. Compact
and semantic observations compose it with family compactors and retained raw
references; clean, silent, and rich modes retain their distinct behavior. Rules
stay configurable through the same `[[compactor]]` TOML format.

### 🛡️ Confinement that fails closed

```sh
confine read-only  -- python analyze.py   # read+run; deny writes/network/common credential paths
confine workspace  -- ./build.sh          # writes only inside $PWD (+ scratch)
confine offline    -- npm test            # network off
confine convert    -- ./thumb.sh          # may exec ONLY `convert`
```

Named leaf presets are kernel-enforced through macOS Seatbelt; Linux Landlock is
planned and unsupported platforms fail closed unless `--best-effort` is explicit.
Credential-path and environment filtering is finite defense in depth, not
complete secret isolation. Sticky command allowlists and `--best-effort` remain
guardrails rather than sandbox boundaries. No LLM makes these decisions.

### ⚡ …and it's still just a shell

Pipelines, lists, functions, here-docs, redirections, and a broad
POSIX/Bash-inspired expansion set are differential-tested against `bash` and
`sh` on every commit, with any intentional
deviation named in the test harness rather than hidden in a pass-rate claim.
`agsh` **never** silently rewrites `ls`, `git`, or `python` into custom
alternatives, and pipes/redirects always receive exact bytes. Plus a fast
themed editor: syntax highlighting as you type, completion dropdown, inline
autosuggestions, reverse search.

## What that looks like day to day

| You want…                                    | Elsewhere                    | In agsh                      |
| -------------------------------------------- | ---------------------------- | ---------------------------- |
| Dev server survives the closed laptop        | tmux/screen ceremony         | `keep -- npm run dev`        |
| Session survives dropped SSH                 | tmux + config                | `agsh --keep` → `--attach`   |
| Recover durable cwd/env/alias records after failure | gone                  | `resume`                     |
| Agent reads a test run                       | full raw dump in context     | `semantic` summary + retained raw reference |
| Restrict a leaf command on supported macOS   | ad hoc wrappers              | `confine read-only -- ./it`  |
| Jump back into yesterday's Claude session    | hunt for the terminal        | `sessions` → `sessions 2`    |

## Install in 60 seconds

**Tagged release assets** use a macOS 11 deployment target for arm64/x86_64 and
an Ubuntu 22.04 build baseline for x86_64/arm64. The qualified support matrix is
currently macOS 15 and Ubuntu 22.04; older macOS versions are best-effort until
they are exercised in CI. The Linux shell is static musl. The installer verifies
checksums, needs no sudo, and targets `~/.local/bin`. Archives also contain
the optional deep-interception library; on Linux that library targets the Ubuntu
22.04 glibc baseline while the shell binary remains static:

```sh
curl --proto '=https' --tlsv1.2 -fsSLo install.sh \
  https://github.com/FusionbaseHQ/agsh/releases/latest/download/install.sh
less install.sh
sh install.sh
```

The installer is itself a checksummed, attested release asset. GitHub CLI users
can verify its provenance with the command in
[`docs/internal/RELEASING.md`](docs/internal/RELEASING.md). Pin the payload with
`AGSH_VERSION=v0.2.0`, change the target directory with `AGSH_INSTALL_DIR`, and
set `AGSH_REQUIRE_ATTESTATION=1` to require the downloaded binary archive's
release-workflow attestation from that exact version tag (this requires a recent
GitHub CLI with `--source-ref` support). A checksum fetched from the same release
detects corruption but is not independent proof of origin. License notices are
retained under `~/.local/share/doc/agsh` (`AGSH_DOC_DIR` overrides this location).
Each release also provides a checksummed and attested corresponding-source
archive with the tagged project source and locked vendored Rust dependencies.

**From source** (the repository-pinned [Rust toolchain](https://rustup.rs)):

```sh
git clone https://github.com/FusionbaseHQ/agsh.git && cd agsh
cargo build --release --locked
install -m755 target/release/agsh target/release/agsh-exec-helper ~/.local/bin/
# Optional experimental :deep interception library, beside the executable:
case "$(uname -s)" in Darwin) ext=dylib ;; Linux) ext=so ;; esac
install -m755 "target/release/libagsh_intercept.$ext" ~/.local/bin/
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
agenv restore --all              # re-apply the exports you've made before, from history
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
| `compact`      | trimmed, deduplicated; tiny outputs skip summary scaffolding |
| `semantic`     | structured JSON observation of recognized commands      |
| `lossless-ref` | status-aware ref; exact only when retained and `complete` |
| `silent`       | suppress display; keep status and a trace when persistence succeeds |
| `rich`         | human rich rendering (see `agview`)                     |

Session defaults apply to **interactive** sessions only — `agsh -c`, scripts,
and pipes stay `raw`, so automation is never silently transformed. Secrets are
redacted from observations by default (`[security]` in `token.toml`).

</details>

<details>
<summary><b>Session resilience — the full model</b></summary>

Three independent layers (see [docs/SESSIONS.md](docs/SESSIONS.md)):

1. **State journal** — every interactive session best-effort appends
   cwd/env/alias/function deltas to a bounded JSONL journal; `resume` replays
   valid retained records. An optional
   startup banner (`[session] restore_banner = true`, off by default) points at
   sessions that likely lost work.
2. **Keep broker** — `agshd` owns PTYs for kept jobs and sessions; terminals
   only ever *detach*. Output is logged (rotated) + a scrollback ring replays
   on attach. Last attach wins; `keep stop` shuts the broker down (and hangs up
   its jobs — documented, deliberate).
3. **Resume recipes** — what can't be kept alive (host death) gets recognized:
   Claude/Codex sessions resurface via `sessions N` with their context intact.

The layers compose: a kept session still journals, so after broker/host death
`resume` can recover the state records that reached durable storage.
Broker process and disk ceilings are described in
[docs/SESSIONS.md](docs/SESSIONS.md); retained output is operational scrollback,
not permanent storage.

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
  agsh-agent/    bounded agent-protocol codec + session path model (server planned)
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
scripts/validate-release.sh
tests/install/run.sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

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

`unsafe` is **forbidden** in every first-party crate except the optional
`agsh-intercept` preload shim, which isolates the required FFI and is never
linked into the shell. The PTY broker itself remains safe Rust. The parser and
executor have regression coverage for deeply nested adversarial inputs, and
security behavior is deterministic by design: no LLM ever makes a sandboxing
decision.

## The promises

1. **Common external commands stay external.** Tools such as `ls`, `git`, and
   `cargo` resolve through `PATH` rather than hidden native replacements.
   Remaining shell-compatibility gaps are tracked explicitly; unsupported
   syntax must fail visibly instead of silently rewriting command behavior.
2. **Pipes and redirects receive exact bytes.** Observation modes affect only
   displayed observations; automatic rich rendering is additionally TTY-gated.
3. **Supported captured commands give agents structure; retained raw remains
   addressable.** Elided observations point to exact bytes only when persistence
   succeeded and the backing trace is marked `complete`; truncation, failure,
   expiry, and disabled storage are explicit. Parsed asynchronous graphs use the
   documented raw fallback until ordered detached capture exists.
4. **Security claims match their boundary.** Supported kernel-backed presets
   fail closed; the deterministic `risk` analyzer and sticky allowlist are
   explicitly advisory guardrails, not sandboxes.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — crates, execution pipeline, design contract
- [Implementation status](docs/IMPLEMENTATION_PLAN.md) — delivered phases, compatibility gaps, production priorities
- [Token economy](docs/TOKEN_ECONOMY.md) — output planes, budgets, retention, active vs reserved config
- [Testing strategy](docs/TESTING_STRATEGY.md) — gates, isolation, regression ownership, remaining gaps
- [Security model](docs/SECURITY_MODEL.md) — trust boundaries, implementation status, release blockers
- [Agent protocol v0 draft](specs/agent-protocol-v0.md) — bounded codec today, authenticated server requirements
- [Session resilience](docs/SESSIONS.md) — journaling, `resume`, the keep broker, wake detection
- [`confine` sandbox](docs/CONFINE.md) — capability sandbox, presets, guarantees
- [Configuration](docs/CONFIGURATION.md) — output modes, config files, environment

## License

Copyright © 2026 Fusionbase and the `agsh` contributors.

Licensed under the **GNU Affero General Public License v3.0 only
(`AGPL-3.0-only`)** — see
[`LICENSE`](LICENSE). The AGPL's network-use clause (section 13) applies: if
you run a modified version of `agsh` and let users interact with it over a
network, you must offer them the corresponding source. Contributions are
accepted under the same license.

Third-party attribution, including the Apache-2.0-licensed rtk-derived reducer
and compactor presets, is recorded in [`NOTICE`](NOTICE), [`LICENSES/`](LICENSES/),
and the generated [`THIRD_PARTY_LICENSES.html`](THIRD_PARTY_LICENSES.html).
Prebuilt archives additionally carry the pinned Rust toolchain's generated
standard-library copyright and license inventory.
