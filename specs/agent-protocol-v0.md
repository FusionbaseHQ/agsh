# Agent protocol v0 draft

Transport targets:

```text
JSONL over stdio
JSONL over Unix domain socket
MCP adapter
OpenAI shell-tool adapter
```

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

```json
{
  "id": "req_01J...",
  "ok": true,
  "result": {}
}
```

Error:

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

## session.open

Request:

```json
{
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

Response:

```json
{
  "session": "sess_01J...",
  "cwd": "/repo",
  "capabilities": ["read:workspace", "write:workspace", "exec:project"],
  "env_policy": "minimal"
}
```

## command.run

Request:

```json
{
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

Events:

```json
{"event":"command_started","cmd_id":"cmd_01J...","argv":["pytest","-q"],"cwd":"/repo"}
{"event":"observation","cmd_id":"cmd_01J...","mode":"semantic","summary":{}}
{"event":"exit","cmd_id":"cmd_01J...","code":1,"duration_ms":3182}
```

## trace.read

Request:

```json
{
  "op": "trace.read",
  "session": "sess_01J...",
  "params": {
    "cmd_id": "cmd_01J...",
    "stream": "stderr",
    "range": {"lines": [100, 160]}
  }
}
```

Response:

```json
{
  "cmd_id": "cmd_01J...",
  "stream": "stderr",
  "range": {"lines": [100, 160]},
  "content": "...",
  "truncated": false
}
```

## file.patch

Request:

```json
{
  "op": "file.patch",
  "session": "sess_01J...",
  "params": {
    "path": "src/auth.py",
    "expected_hash": "sha256:...",
    "patch": "..."
  }
}
```

Response:

```json
{
  "applied": true,
  "new_hash": "sha256:...",
  "diff_ref": "trace://patch_01J.../diff"
}
```
