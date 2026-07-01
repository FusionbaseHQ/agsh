# Security Policy

`agsh` is a shell and ships a capability sandbox (`confine`). We take security
reports seriously.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately via GitHub's [Security Advisories](https://github.com/FusionbaseHQ/agsh/security/advisories/new),
or email the maintainers. Include:

- affected version / commit,
- platform and OS version,
- a description and, ideally, a minimal reproduction.

We aim to acknowledge within a few business days and to coordinate a fix and
disclosure timeline with you.

## Scope

Of particular interest:

- **`confine` bypasses** — any way a confined payload gains access it was denied
  (writes/network/exec/secret reads), or a case where `confine` runs a payload
  *unconfined* instead of failing closed.
- Stream-corruption bugs where piped/redirected bytes are altered.
- Parser/executor crashes or memory-safety issues (note: `unsafe` is forbidden in
  first-party crates).

## Non-goals

`confine` is a capability boundary, not full isolation. It does not defend against
kernel/LSM vulnerabilities or side channels, and on platforms without a supported
backend it **fails closed** (refuses to run) rather than pretending to confine.
See [`docs/CONFINE.md`](docs/CONFINE.md).
