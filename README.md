# xses

`xses` lets you find, resume, and share coding-agent sessions across
Claude Code, Codex, Pi, oh-my-pi, and OpenCode.

## Install

```sh
cargo install --path . --root "$HOME/.local"
```

The command is `xses`.

## Quick start

```sh
xses                     # pick a session for the current directory
xses sync                # sync the newest session from each agent
xses sync -n 5           # sync the newest five from each agent
xses sync --all          # sync every native session, from every directory
xses sync --dry-run      # preview without writing anything
xses ls                  # list sessions
xses clean               # remove xses imports
```

By default, sync is limited to the current directory and selects one newest
session per source agent. Use `-n N` for the newest N sessions per agent, or
`--all` for the complete history across all directories. You can also target a
specific destination:

```sh
xses sync --to pi
xses sync latest --to claude
```

## What it does

- Shows sessions from all supported agents in one searchable picker.
- Resumes a session in the agent that owns it.
- Imports sessions into other agents without modifying the source.
- Skips empty sessions and xses-created imports.
- Avoids duplicate imports and reports preserved or changed content.
- Supports `--dry-run`, limited sync with `--scope`, and reversible cleanup.

Supported agents:

| Agent | Resume command |
|---|---|
| Claude Code | `claude --resume ID` |
| Codex | `codex resume ID` |
| Pi | `pi --session ID` |
| oh-my-pi | `omp -r ID` |
| OpenCode | `opencode -s ID` |

Session data is read from each agent's local storage. OpenCode sessions are
read from its local database and imported through its native import interface.

## Useful commands

```sh
xses pick -q "query" --all
xses ls --count
xses sync latest --to pi --dry-run
xses sync --scope 20
xses clean --dry-run
```

Session lists and resume commands are usually scoped to the current directory.
Run `xses sync --all` when you need sessions from other projects.

## License

MIT. See [LICENSE](LICENSE).
