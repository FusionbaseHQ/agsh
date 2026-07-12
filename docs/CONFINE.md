# `confine` — capability sandbox

On a supported platform, `confine PRESET -- COMMAND` runs a leaf command under a
kernel-enforced restriction of what it may touch: the filesystem, the network,
and which programs it may execute. It is a hard floor *beneath* whatever
permissions the caller already has. Linux enforcement is not implemented yet and
therefore refuses; the sticky and `--best-effort` forms are weaker guardrails.

```sh
confine read-only -- python analyze.py    # no writes/network; deny common credential paths
confine workspace -- ./build.sh           # writes only within $PWD (+ a private scratch dir)
confine offline  -- npm test              # network off; filesystem unchanged
confine convert,identify -- ./thumb.sh    # exec-allowlist: may only run convert / identify
confine read-only --rw ./out -- ./tool    # read-only, except ./out is writable
```

## Presets

| Preset       | Filesystem                    | Network | Notes                                  |
| ------------ | ----------------------------- | ------- | -------------------------------------- |
| `read-only`  | read except listed credential paths; no writes | off | finite denylist, not exhaustive |
| `workspace`  | writes only within `$PWD`     | off     | plus a private scratch dir             |
| `offline`    | unchanged                     | off     | just cuts the network                  |
| exec-allowlist (`a,b -- …`) | inherited        | inherited | payload may only `exec` `a` or `b`   |

`read-only` and `workspace` deny network and a finite set of common credential
paths by default (for example `~/.ssh/id_rsa`). This is defense in depth rather
than general secret isolation. A private scratch directory is provided so tools
that need temporary files keep working.

## Flags

| Flag             | Effect                                                        |
| ---------------- | ------------------------------------------------------------ |
| `--rw PATH`      | add a writable root                                          |
| `--net`/`--no-net` | force network on/off                                        |
| `--explain`      | print the capabilities that will be granted/denied, then run |
| `--dry-run`      | print the capability summary + wrapped command; do not run    |
| `--force`        | bypass only the self-managing-agent refusal                   |
| `--best-effort`  | fall back to a shim layer when no OS backend is available     |
| `--allow-secrets`| do not deny the preset's known secret paths                   |

## Confining the current session

```sh
confine ls,df        # restrict *this* shell to an exec-allowlist (sticky for the session)
```

> **This is a convenience guardrail, not a security boundary.** The sticky
> allowlist (and the `AGSH_CONFINE` env it propagates to child `agsh`) matches on
> command basename inside agsh's own resolver. It is bypassable — by an
> absolute-path interpreter (`/bin/bash -c …`), by any allowlisted command that
> can itself spawn others (`env`, `xargs`, `find -exec`, `python`), and by a
> payload that clears or widens `AGSH_CONFINE`. Use it to catch honest mistakes;
> for an actual boundary use a kernel-enforced preset (`confine read-only -- …`)
> on a supported platform.

## Self-managing agents are refused

Interactive agents that manage their own permissions (e.g. `claude`) are refused
with guidance to use their own tool-permission systems: their broad runtime can't
be reduced to a small, meaningful allowlist, so confining them gives a false sense
of safety. `--force` overrides this.

## Platform support

| Platform | Backend            | Status                                             |
| -------- | ------------------ | -------------------------------------------------- |
| macOS    | Seatbelt (`sandbox-exec`) | Supported                                    |
| Linux    | Landlock LSM       | Planned; `confine` currently **fails closed** (refuses rather than running unconfined) |
| other    | —                  | Fails closed                                        |

Without `--best-effort`, `confine` never runs a payload it cannot actually
confine. If no enforcing backend is available it refuses. `--best-effort` is an
explicit downgrade to command-name shims: it does **not** enforce filesystem,
network, secret-read, environment, or cross-process restrictions, even when the
invocation names `read-only`, `workspace`, or `offline`.

## Guarantees & limits

- **Narrow-only:** confinement can only remove access, never add it.
- **Fail-closed:** no backend ⇒ refuse, don't silently run unconfined.
- **Interpreter-safe:** the restriction is enforced by the OS on the whole process
  tree, so `confine read-only -- python evil.py` cannot delete files even though
  Python's own APIs are used — the earlier "interpreter bypass" class is closed.
- **Not a VM:** `confine` is a capability boundary, not full isolation. It does not
  defend against kernel/LSM vulnerabilities, side channels, or resource exhaustion
  beyond the limits it sets. Network denial covers TCP/UDP **and AF_UNIX sockets**
  via the OS backend.

### Non-guarantees (read before you rely on it)

- **The session/`AGSH_CONFINE` allowlist is a nudge, not a boundary** (see
  [Confining the current session](#confining-the-current-session)). Only the
  kernel-enforced `confine PRESET -- CMD` path is a real boundary, and only on a
  supported platform.
- **Secret protection is best-effort.** Reads of known credential paths are denied
  and common AWS credential names, sensitive suffixes such as `*_TOKEN` /
  `*_SECRET`, and `SSH_AUTH_SOCK` are scrubbed under `read-only` / `workspace`, but
  neither list is exhaustive, and the presets deny *reads* — they do **not**
  prevent a payload from *writing* to credential or autorun files
  (`~/.ssh/authorized_keys`, `~/.zshrc`,
  `.git/hooks/*`, `.envrc`) inside a writable root. Don't point `workspace` at
  `$HOME`.
- **Network deny is not airtight against confused-deputy IPC.** TCP/UDP and unix
  sockets are denied, but a Foundation-capable interpreter can still ask a
  mach-mediated system daemon (e.g. `nsurlsessiond`) to act on its behalf. Treat
  network denial as defense-in-depth, not a hermetic seal.
- **`--run --allow` / exec-only restrict *exec only*.** They do not restrict the
  filesystem, network, secret reads, or cross-process access — use a named preset
  when you need those.
- **An executable allowlist is not a syscall allowlist.** `/bin/sh` is required as
  the Seatbelt launcher (plus its `/bin/bash` executable variant on macOS), and
  any explicitly allowed interpreter or programmable tool can perform work
  in-process without another `exec`. Combine the exec list with a named
  filesystem/network preset when the payload is untrusted.
- **Traces are not confined output.** Raw command output persisted to
  `$AGSH_TRACE_DIR` is stored unredacted (files are `0600`); it is not subject to
  the sandbox's secret-read denials.
