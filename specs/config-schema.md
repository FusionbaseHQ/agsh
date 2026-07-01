# Config schema draft

## User config

Path:

```text
~/.config/agsh/config.toml
```

Example:

```toml
[shell]
default_mode = "native"
posix_compat = true
startup_budget_ms = 5

[compat]
external_coreutils_by_default = true
accelerated_coreutils = false
path_cache = true

[env]
path_as_list = true
load_dotenv = false

[output]
human_default = "raw"
agent_default = "semantic"
store_raw = true

[security]
project_config_requires_trust = true
agent_default_policy = "agent.workspace"
```

## Token config

Path:

```text
~/.config/agsh/token.toml
./.agsh/token.toml
```

See `docs/TOKEN_ECONOMY.md`.

## Project trust

Project-local startup files must require explicit trust:

```sh
agsh trust project
```

Trust records should include:

```text
workspace path
canonical device/inode where possible
git remote hash where applicable
user principal
timestamp
config hash
```
