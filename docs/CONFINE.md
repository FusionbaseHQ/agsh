# `confine` — capability sandbox

`confine` runs a command (or the current shell session) under a **kernel-enforced**
restriction of what it may touch: the filesystem, the network, and which programs
it may execute. It is a hard floor *beneath* whatever permissions the caller
already has — a confined process can only ever have **less** access, never more.

```sh
confine read-only -- python analyze.py    # read + run; no writes, no network, no secret reads
confine workspace -- ./build.sh           # writes only within $PWD (+ a private scratch dir)
confine offline  -- npm test              # network off; filesystem unchanged
confine convert,identify -- ./thumb.sh    # exec-allowlist: may only run convert / identify
confine read-only --rw ./out -- ./tool    # read-only, except ./out is writable
```

## Presets

| Preset       | Filesystem                    | Network | Notes                                  |
| ------------ | ----------------------------- | ------- | -------------------------------------- |
| `read-only`  | read anywhere, no writes      | off     | secret paths (`~/.ssh`, …) unreadable  |
| `workspace`  | writes only within `$PWD`     | off     | plus a private scratch dir             |
| `offline`    | unchanged                     | off     | just cuts the network                  |
| exec-allowlist (`a,b -- …`) | inherited        | inherited | payload may only `exec` `a` or `b`   |

`read-only` and `workspace` deny network **and** secret reads by default, so a
script can't both read `~/.ssh/id_rsa` and exfiltrate it. A private scratch
directory is provided so tools that need temporary files keep working.

## Flags

| Flag             | Effect                                                        |
| ---------------- | ------------------------------------------------------------ |
| `--rw PATH`      | add a writable root                                          |
| `--net`/`--no-net` | force network on/off                                        |
| `--explain`      | print the capabilities that will be granted/denied, then run |
| `--dry-run`      | print the resolved profile and exit without running          |
| `--force`        | run even a payload that would otherwise be refused           |
| `--best-effort`  | fall back to a shim layer when no OS backend is available     |

## Confining the current session

```sh
confine ls,df        # restrict *this* shell to an exec-allowlist (sticky for the session)
```

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

`confine` never runs a payload it cannot actually confine. If no enforcing backend
is available it refuses (unless you opt into `--best-effort`).

## Guarantees & limits

- **Narrow-only:** confinement can only remove access, never add it.
- **Fail-closed:** no backend ⇒ refuse, don't silently run unconfined.
- **Interpreter-safe:** the restriction is enforced by the OS on the whole process
  tree, so `confine read-only -- python evil.py` cannot delete files even though
  Python's own APIs are used — the earlier "interpreter bypass" class is closed.
- **Not a VM:** `confine` is a capability boundary, not full isolation. It does not
  defend against kernel/LSM vulnerabilities, side channels, or resource exhaustion
  beyond the limits it sets. Network denial covers TCP/UDP via the OS backend.
