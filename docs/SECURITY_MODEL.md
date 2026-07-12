# Security model and implementation status

Security-sensitive behavior in agsh must be deterministic. No language model is
used to classify a command, grant a capability, choose a sandbox, redact a
secret, or approve an operation. Deterministic does not mean complete: static
analysis and name-based redaction have explicit limits below.

## Trust boundaries

### Local interactive shell

The normal shell runs with the invoking user's authority. It is not a privilege
boundary and does not authenticate the human at the terminal. Normal commands
inherit the shell's exported environment and OS permissions, as users expect.

Project `.env` activation is fail-closed and requires an explicit `agtrust` for
the file's current versioned SHA-256 digest. The trust database and `.env` input
are bounded regular files opened nonblocking without following final symlinks;
an unreadable or non-persistable trust database never activates the variables.
SHA-256 is an edit-resistant trust key, not a signature of the project author.

### Output observations

Compact and semantic observations use deterministic secret redaction. Raw child
streams, redirected files, pipes, and successfully persisted `complete` traces
remain exact bytes and can contain secrets. Trace persistence is best effort;
references may be disabled, truncated, or expire, and `agtrace` exposes bounded
status-aware views. Trace files are private (`0600`) but any process running as
the same OS user may generally read them. Redaction is not data-loss prevention.

### Session journal and keep broker

Session journals and broker logs are unredacted same-user state and can contain
commands, output, paths, and exported secrets. Their directories/files are
private and broker control sockets verify peer UID, but same-UID processes are
inside this trust boundary. Journal appends are best effort and not transactional;
`resume` must not be used to preserve or re-establish a security policy.

Broker control frames, connection count, tail responses, and per-job log
generations are bounded. The total number of running/retained jobs, accumulated
old job logs, and daemon-log bytes are not globally bounded yet; see
[`SESSIONS.md`](SESSIONS.md) for current limits and cleanup requirements.

### Risk analysis and command-name confinement

`risk` statically flags a high-signal subset of dangerous command forms. An empty
result does not prove safety: expansion, aliases/functions, compound syntax,
interpreters, plugins, and arbitrary executable behavior cannot be exhaustively
classified from argv. `risk` is advisory unless a trusted caller separately feeds
its findings into policy evaluation.

The sticky `confine LIST` / `AGSH_CONFINE` command allowlist is also a guardrail,
not a boundary. It can only govern launches routed through agsh and can be
bypassed by a capable already-running process or programmable allowed tool.

### Kernel-backed confinement

`confine PRESET -- COMMAND` is the security-boundary form:

| Platform | Current backend | Current behavior |
| --- | --- | --- |
| macOS | `/usr/bin/sandbox-exec` / Seatbelt | named presets are wrapped in an OS profile |
| Linux | none | refuses unless `--best-effort` is explicit |
| other | none | refuses unless `--best-effort` is explicit |

`--best-effort` only installs command-name shims. It does not enforce the named
preset's filesystem, network, secret, environment, or process restrictions. See
[`CONFINE.md`](CONFINE.md) for the Seatbelt profile and platform-specific limits.

Linux Landlock, namespaces, seccomp, cgroups, and network isolation are not
implemented. Ubuntu strict-mode confinement is therefore not release-ready; the
current safe behavior is refusal, not silent execution without a sandbox.

### Agent protocol

`agsh-agent` is currently a bounded JSONL codec and session path model. There is
no agent server, authentication, session ownership, dispatcher, approval store,
execution-environment sanitizer, Unix-socket transport, or MCP adapter. The draft
must not be exposed as a remote execution interface. See
[`agent-protocol-v0.md`](../specs/agent-protocol-v0.md) for required controls.

## Deterministic policy

Capabilities and principals use bounded, delimiter-safe identifiers. Policy
evaluation is default-deny for capabilities outside an agent mode's baseline,
requires approval at deterministic risk thresholds, and denies critical findings
for every agent mode. This is decision logic only. A trusted operation handler
must derive required capabilities, enforce the resulting decision, and bind any
approval to the exact principal, command/operation, cwd, capability, and target.

Callers must never accept a requester's own list of "required capabilities" as
authoritative. A command can omit or misdescribe its effects.

## Secrets

The macOS `read-only` / `workspace` presets deny a finite list of common
credential paths and remove known credential plus loader/interpreter-injection
variables. This list is defense in depth, not exhaustive discovery. Secret values
embedded under unusual names, inherited file descriptors, command arguments,
files inside an allowed
workspace, keychain/Mach services, and same-user processes remain relevant attack
surfaces.

Future agent sessions must construct a minimal environment from an allowlist;
filtering a full developer environment by a denylist is insufficient. In
particular, do not inherit `HOME`, `SSH_AUTH_SOCK`, cloud credentials, token or
password variables, or loader startup hooks by default.

## Release blockers for security claims

- Implement and adversarially test a Linux kernel backend before advertising
  Ubuntu strict-mode sandboxing.
- Implement authenticated protocol transports, server-generated session secrets,
  per-operation capability derivation, bounded streaming, cancellation, and
  descriptor-relative file handlers before shipping the agent server.
- Add an approval-record store with exact scope, expiration, auditability, and
  revocation before approvals can authorize commands.
- Test macOS profiles on every supported OS version and keep Seatbelt limitations
  explicit; it is not a VM and does not stop resource exhaustion or all
  confused-deputy IPC.
- Add adversarial tests for inherited descriptors, environment secrets, symlink
  and rename races, trace authorization, Unix sockets, process inspection, and
  resource exhaustion.
