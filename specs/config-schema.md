# Future config schema (design draft)

> **Not implemented in agsh 0.2.** This document describes a possible future
> general/project policy schema. agsh does not load `config.toml`, policy TOML,
> or project-local token configuration today. The active runtime contract is
> [`docs/CONFIGURATION.md`](../docs/CONFIGURATION.md): user `token.toml`, an
> interactive `agshrc`, environment variables, and explicit builtins/flags.

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

## Proposed project token config

Path:

```text
./.agsh/token.toml
```

See `docs/TOKEN_ECONOMY.md`.

## Project trust

Any future project-local configuration must require explicit trust. Proposed
syntax (not an available 0.2 command) was:

```sh
agsh trust project  # proposed only
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
