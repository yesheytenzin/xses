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
xses doctor                   # installed stores, index, newest failures
```

`sync` tokens are destinations (tool names, `sharegpt`, `openai`) or session
refs (`latest`, `latest:N`, an id prefix); `-n N` moves the newest N sessions
per source tool (default 1), `--scope N` imports only the trailing N turns,
`--dry-run` writes nothing, and `--include-imports` also selects untouched
imported copies. A ref-only `sync` is a preview.

After you continue an imported copy with new user, assistant, image, or tool activity, ordinary `sync`
includes it automatically (within the same newest-per-agent limit). It sends
the full continued transcript back as a separate branch; it never merges into
or overwrites the original native session. Repeated syncs update that branch
in place. Untouched returned copies are excluded so history does not echo
between agents. Titles retain the original `[agent]` prefix.

If a target copy has changed since import, refreshing it fails with a
`continued copy ... was preserved` message instead of erasing its work. Sync
that copy by ID to share its branch, then resume the branch you want to use.
Detection uses native message IDs and semantic block fingerprints, including
repeated prompts. Title/metadata changes alone do not trigger a return branch,
but changed history is still protected from replacement. Older user-only
baselines and imports without a baseline need explicit selection by session ID.
Sync reports unreadable baselines per session and continues healthy work.
Mapping 14 creates fresh tracked copies on upgrade; older copies with unknown
or changed history are retained until you explicitly remove them.

`clean` preserves changed copies and copies with missing or unreadable baselines.
Use `clean --dry-run` to preview this; `clean --force` explicitly removes these
copies too. Interrupted import publications recover before the next mutating
sync or clean. Stop work in a target while replacing it: native agents do not
participate in xses's operation lock.

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
artifacts), scan/count caches, `issues.jsonl`, `import-baselines/` (message IDs
and fingerprints), and `pending-imports/` (durable recovery journals). Recovery
backups are removed after successful publication. Scan caches are versioned and
checkpoint complete JSONL records; unchanged listings do not rewrite them.
Source-filtered commands only scan the requested stores. 
Maintainers release with
`scripts/release.sh --help`.

## License

MIT. See [LICENSE](LICENSE).

## Development checks

Rust 1.89 or newer is required. Run `cargo test --all-targets --locked`,
`cargo clippy --all-targets --locked -- -D warnings`, and `cargo fmt --check`.
PR CI runs these on Linux, macOS, and Windows. On Linux, run the isolated CLI
regressions with `python3 verify-xses/flows/regression.py target/debug/xses`
after `cargo build`; Python 3 and `sqlite3` must be on PATH.
Run `python3 verify-xses/flows/continuations.py target/debug/xses` for continued
imports, return branches, echo prevention, and conflicting local work in the
Claude, Codex, Pi, and oh-my-pi file stores.
Run `python3 verify-xses/flows/safety.py target/debug/xses` for failure injection,
cleanup ordering, missing artifacts/baselines, and interrupted publication.
With initialized agent CLIs, `python3 verify-xses/flows/native.py target/debug/xses`
checks native Pi export, Codex loading, and OpenCode replacement/cleanup.

For repeatable listing timings, build with `cargo build --release --locked`
and run `python3 verify-xses/flows/benchmark.py`. It reads native stores and
writes only a disposable cache under `/tmp`; it does not measure cold disks.
Pass `--baseline /path/to/old/xses` for a comparison.
