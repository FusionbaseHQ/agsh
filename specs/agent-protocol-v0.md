# Agent protocol v0 draft

## Implementation status

This document is a **draft wire contract**, not a shipped remote-execution
service. `agsh-agent` currently provides:

- a strict, bounded codec for one JSONL envelope at a time;
- validated envelope request/session ids, outbound event command ids, and a typed
  operation vocabulary;
- a canonical workspace root and existing-path containment checks;
- bounded response/event encoding and token-budget validation.

It does **not** provide a stdio or Unix-socket server, authentication, session
ownership, operation handlers, command cancellation, trace authorization,
descriptor-relative file access, an MCP adapter, or an approval-record store. Do
not expose the codec directly to an untrusted peer. Those are release blockers for
any agent-server feature, even though the local shell can ship without that
feature.

Target transports (not implemented):

```text
JSONL over stdio
JSONL over Unix domain socket
MCP adapter
OpenAI shell-tool adapter
```

## Framing and validation

- One frame is at most 1 MiB, including an optional final newline.
- Exactly one JSON value is accepted. Interior literal CR/LF bytes, duplicate
  envelope fields, and unknown envelope fields are rejected.
- Envelope `id` / `session` and outbound event command ids are 1-128 ASCII bytes
  containing only letters, digits, `_`, `-`, `.`, or `:`. Future operation
  handlers must apply the same rule to ids nested inside `params`.
- `params` is required and must be a JSON object.
- `session.open` must omit `session`; every other v0 operation must include it.
- Unknown operations are rejected before dispatch.
- Transports must apply the same size bound while reading, before accumulating an
  unbounded line.

## Request envelope

```json
{
  "id": "req_01J...",
  "op": "command.run",
  "session": "sess_01J...",
  "params": {}
}
```

## Response envelope

Success has exactly one `result` value:

```json
{
  "id": "req_01J...",
  "ok": true,
  "result": {}
}
```

Failure has exactly one `error` value:

```json
{
  "id": "req_01J...",
  "ok": false,
  "error": {
    "kind": "PolicyDenied",
    "message": "network access requires approval",
    "details": {}
  }
}
```

## Session and authorization requirements

A future transport must bind every session to an authenticated connection or
principal. Possession of a caller-selected session id is not authentication.
Session ids must be server-generated with sufficient entropy and compared within
the authenticated principal's namespace. Reconnect/resume needs a separate,
rotatable secret rather than a predictable id.

Handlers, not requesters, derive the capabilities required by each operation.
`agsh-policy::evaluate_policy` only evaluates those trusted declarations and
static findings; it neither discovers side effects nor enforces a sandbox.
Approval records must bind principal, session, operation/command hash, cwd,
capability, and target, and must expire or be explicitly revoked.

Agent children must start from an allowlisted environment. In particular they
must not inherit `HOME`, `SSH_AUTH_SOCK`, loader/interpreter injection variables,
cloud credentials, or token/password variables merely because the server has
them. This execution environment is not implemented by `agsh-agent` yet.

## Workspace path requirements

The session model accepts only an existing canonical directory as its workspace.
Existing read targets reject absolute paths, `..`, and symlinks resolving outside
that root. This is a containment check, not a race-free file API. Future
`file.read_range` and `file.patch` handlers must use descriptor-relative opens
with no-follow semantics and revalidate file identity; checking a path and later
opening it by name is vulnerable to TOCTOU replacement.

## Operation vocabulary

The codec recognizes these names so unknown operations fail deterministically;
recognition does not mean a handler exists:

```text
session.open
command.run
command.input
command.cancel
trace.read
file.read_range
file.patch
git.diff
git.snapshot
```

### `session.open` draft

```json
{
  "id": "req_01J...",
  "op": "session.open",
  "params": {
    "workspace": "/repo",
    "principal": "agent.codex",
    "policy": "agent.workspace",
    "output": {
      "mode": "semantic",
      "token_budget": 2000,
      "store_raw": true
    }
  }
}
```

### `command.run` draft

```json
{
  "id": "req_01J...",
  "op": "command.run",
  "session": "sess_01J...",
  "params": {
    "cmd": "pytest -q",
    "output": {
      "mode": "semantic",
      "token_budget": 2500
    }
  }
}
```

Event sequence:

```json
{"event":"command_started","cmd_id":"cmd_01J...","argv":["pytest","-q"],"cwd":"/repo"}
{"event":"observation","cmd_id":"cmd_01J...","body":{}}
{"event":"exit","cmd_id":"cmd_01J...","code":1,"duration_ms":3182}
```

### `trace.read` draft

```json
{
  "id": "req_01J...",
  "op": "trace.read",
  "session": "sess_01J...",
  "params": {
    "cmd_id": "cmd_01J...",
    "stream": "stderr",
    "range": {"lines": [100, 160]}
  }
}
```

### `file.patch` draft

```json
{
  "id": "req_01J...",
  "op": "file.patch",
  "session": "sess_01J...",
  "params": {
    "path": "src/auth.py",
    "expected_hash": "sha256:...",
    "patch": "..."
  }
}
```
