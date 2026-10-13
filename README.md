# xsession

`xsession` lets you find, resume, and share coding-agent sessions across
Claude Code, Codex, Pi, oh-my-pi, and OpenCode.

![xses unified session picker](docs/images/xses-picker.png)

Scan → extract → write, per store:

![xses workflow](docs/images/xses-workflow.png)

## Install

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://raw.githubusercontent.com/yesheytenzin/xsession/main/install.sh | sh
```

The installer detects the platform, verifies the release checksum, and puts
`xses` in `~/.local/bin`. From source:
`cargo install --path . --root "$HOME/.local"`.

## Commands

Every command works on the current project; `--all` widens it to every project.

```sh
xses                          # pick a session here and resume it
xses pick -q "query" --all    # fuzzy search over every project
xses ls                       # newest 5 sessions in this project
xses ls --count               # per-tool tallies, plus hidden-session counts
xses sync                     # newest session from each agent -> every other agent
xses sync --from claude --to pi
xses sync latest --to codex   # one session by ref
xses sync latest sharegpt -o chat.json
xses clean                    # remove the newest synced session's imports
xses clean --all --dry-run    # report every import that would go
xses doctor                   # stores, index, and the newest failures
```

`sync` tokens are destinations (tool names, `sharegpt`, `openai`) or session
refs (`latest`, `latest:N`, an id prefix); `-n N` moves the newest N sessions
per source tool (default 1), `--scope N` imports only the trailing N turns,
`--dry-run` writes nothing, and `--include-imports` re-imports files xses
itself wrote. A ref-only `sync` is a preview.

Long syncs report progress on stderr — terminals only, silenced by
`XSES_NO_PROGRESS=1`.

## Agents

| Agent | Source | Resume |
|---|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` | `claude --resume ID` |
| Codex | `~/.codex/sessions/**/*.jsonl` + state db | `codex resume ID` |
| Pi | `~/.pi/agent/sessions/**/*.jsonl` | `pi --session ID` |
| oh-my-pi | `~/.omp/agent/sessions/**/*.jsonl` | `omp -r ID` |
| OpenCode | its local SQLite store | `opencode -s ID` |

Imports never modify the source. They carry the transcript's structure — tool
calls and results, reasoning, images, patches, token/cost accounting — into
each target's own native constructs, are idempotent per session content, and
open with a note that the history already ran elsewhere. Whatever a target
cannot hold (stripped reasoning, oversized images, live state) becomes a
visible `[gap in imported history: …]` marker. Sessions that were never useful
(`empty`, `unanswered`, `failed`) stay hidden unless `--include-noise`. What
transfers per data class is in [FEATURE.md](FEATURE.md).

State lives in `~/.local/share/xses/`: `index.jsonl` (imports and their
artifacts), `scan-cache.json`, `issues.jsonl`. Maintainers release with
`scripts/release.sh --help`.

## License

MIT. See [LICENSE](LICENSE).
