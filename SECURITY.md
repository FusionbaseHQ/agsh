# Security Policy

`agsh` is a shell and ships a capability sandbox (`confine`). We take security
reports seriously.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately via GitHub's [Security Advisories](https://github.com/FusionbaseHQ/agsh/security/advisories/new).
Include:

- affected version / commit,
- platform and OS version,
- a description and, ideally, a minimal reproduction.

We aim to acknowledge within a few business days and to coordinate a fix and
disclosure timeline with you.

## Supported versions

Security fixes are applied to `main` and, once public releases exist, to the
latest GitHub release line. Older release lines and arbitrary forks are not
maintained by this project.

## Scope

Of particular interest:

- **`confine` bypasses** — any way a confined payload gains access it was denied
  (writes/network/exec/secret reads), or a case where `confine` runs a payload
  *unconfined* instead of failing closed.
- Stream-corruption bugs where piped/redirected bytes are altered.
- Parser/executor crashes or memory-safety issues. `unsafe` is forbidden in
  ordinary first-party crates; executable-boundary operations are isolated in
  the optional `agsh-intercept` preload library and the one-call `agsh-signal`
  SIGPIPE reset wrapper.

## Non-goals

`confine` is a capability boundary, not full isolation. It does not defend against
kernel/LSM vulnerabilities or side channels, and on platforms without a supported
backend it **fails closed** (refuses to run) rather than pretending to confine.
See [`docs/CONFINE.md`](docs/CONFINE.md).

Deep shell interception is an experimental observation aid, not a security
boundary. Platform loaders can omit it (including SIP/hardened, static, and
setuid execution), and the preload hooks are not guaranteed safe in every
post-fork, multi-threaded child. Security policy must rely on `confine` and its
documented platform backend, never on `AGSH_INTERCEPT=...:deep` coverage.
