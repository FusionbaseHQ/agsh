# Configuration

`agsh` reads its configuration from `~/.config/agsh/`. Everything has a sensible
default; nothing is required. Example files live in [`configs/agsh/`](../configs/agsh).

## Files

| File                        | Purpose                                             |
| --------------------------- | --------------------------------------------------- |
| `~/.config/agsh/token.toml` | output modes and token-economy defaults             |
| `~/.config/agsh/config.toml`| general shell settings                              |
| `~/.config/agsh/policies/`  | `confine`/allowlist policy files                    |

> **Startup rc file:** an [`agshrc.example`](../configs/agsh/agshrc.example) ships
> as a template, but automatic sourcing at startup is not wired up yet. For now,
> load your aliases/functions/prompt manually with `source ~/.agshrc`. Automatic
> rc autoload is on the roadmap.

Copy an example to get started:

```sh
mkdir -p ~/.config/agsh
cp configs/agsh/token.toml ~/.config/agsh/token.toml
```

## Output modes

Every command can be rendered in a mode. Priority, highest first:

1. per-command wrapper — `semantic git diff`
2. `--output` flag — `agsh --output compact -c 'pytest -q'`
3. `mode` builtin (session default) — `mode:output compact`
4. `AGSH_OUTPUT_MODE` environment variable
5. `~/.config/agsh/token.toml` `[mode] default`
6. built-in default — `raw`

```toml
# ~/.config/agsh/token.toml
[mode]
default = "compact"   # applies to interactive sessions only
```

The config / `mode` default applies to **interactive** sessions only —
non-interactive `agsh -c` and scripts stay `raw`, so piped output is never
silently transformed.

| Mode           | Use                                                     |
| -------------- | ------------------------------------------------------- |
| `raw`          | default; exact bytes, streamed                          |
| `compact`      | trimmed, deduplicated output                            |
| `semantic`     | structured observation of recognized commands           |
| `lossless-ref` | compact view + a `trace://` reference to the raw stream |
| `silent`       | suppress display, keep exit status + trace              |
| `rich`         | human rich rendering (see `agview`)                     |

### The `mode` builtin

```sh
mode:output compact   # set the output aspect (mode compact is a shorthand)
mode:output           # show one aspect
mode                  # show all aspects
mode:output off       # reset to the startup default
```

## Environment variables

| Variable            | Effect                                                    |
| ------------------- | -------------------------------------------------------- |
| `AGSH_OUTPUT_MODE`  | default output mode for the session                       |
| `AGSH_ICONS=1`      | enable Nerd Font glyphs in the UI                         |
| `NO_COLOR`          | disable color (honored)                                   |

## Themes

Colors adapt to the terminal's detected capability (truecolor / 256 / 16). Palette
and role mapping are configurable; see [`THEMING`](../configs/agsh) examples and the
`agsh-style` crate for the role vocabulary.
