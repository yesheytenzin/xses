# Changelog

## Unreleased — structured transfer (mapping 2)

Imports now carry the source's own structure instead of flattening everything
to text. A source message maps to one native message, and its parts map to the
target's constructs:

- tool calls and results travel as `tool_use`/`tool_result` (claude),
  `function_call`/`function_call_output` (codex), `toolCall`/`toolResult`
  (pi/omp), `tool` parts (opencode) — with correlation ids, error state, and
  durations, so a resumed agent sees what ran and what it returned.
- reasoning travels as `thinking` / `reasoning` parts (text only: provider
  signatures are never replayed).
- images travel inline (base64/`input_image`/`image` parts) up to 5 MB per part
  and 20 MB per session; larger ones become named gaps.
- codex projections carry `commandExecution` rows (with output) and usage
  arrives as `token_count` events; opencode gets `reasoning`, `tool`, and
  `patch` parts plus `step-finish` tokens; claude gets per-message `usage`.
- every import opens with a note that the transcript already ran elsewhere, so
  tool traffic is read as history, never continued as pending state.
- images also reach opencode, as `file` parts carrying the data URL (verified
  against the live importer; its schema is `type/mime/filename/url`).
- opencode imports are serialized: its CLI opens SQLite with `busy_timeout=0`,
  so a second writer fails instead of waiting. Retries back off three times for
  the case where another app is writing.
- `sync <id>` no longer reports "ambiguous" just because the id also exists in
  stores it was imported into: the original session wins, then the newest.
- import headers carry a manifest (`blocks`, `gaps`, `mapping`), the mapping
  version is part of the idempotency fingerprint (older copies re-import once),
  and `FEATURE.md` documents what is carried, text-only, or impossible.

Also fixed along the way: `ls --all -n N` ignored the explicit `-n`
(`scope::limit`), and the release script's `--help` leaked the `set -euo` line.

## 0.5.0 — release tooling and transfer docs

- `scripts/release.sh <X.Y.Z>`: bumps the version in `Cargo.toml`, `Cargo.lock`,
  and the changelog, runs the `--locked` gates, commits, tags, and pushes.
  Pushing the tag is what builds the release (linux-x86_64, macos-x86_64,
  macos-aarch64, windows-x86_64 + `SHA256SUMS` and signed provenance).
- `FEATURE.md` documents what a sync can carry, what is planned, and what can
  never transfer (provider-signed reasoning, redacted content, live state).
- Install references point at the canonical `yesheytenzin/xsession` name.

## 0.0.4 — CLI contract, diagnostics, and faster syncs

- `doctor` reports what is installed (agent CLIs, `sqlite3`), what each store
  holds, how many imports the index records (including records whose artifacts
  are gone), and the newest failures from a persistent issue journal
  (`~/.local/share/xses/issues.jsonl`).
- Failed imports are journaled with their cause instead of only printing once:
  `opencode import` errors (including the CLI's own message) and codex history
  seeding failures now show up in `doctor`.
- `opencode import` retries once, and retries take a lock so only one runs at
  a time: concurrent imports lose SQLite lock races and exit with the CLI's
  generic "Unexpected error" (~15% of a 200-session batch at 8-way
  parallelism). Serialized retries took a 916-session backlog from 16
  failures to 0.
- Writers moved to `src/write/` (one module per target), reporting a
  `WriteOutcome`; `scope_last_n` moved to `canonical.rs`; the duplicated
  `command_output_timeout` and JSONL helpers are now `proc.rs` and `jsonl.rs`.
- `ls` and `pick` show the newest 5 sessions of the current project; `-n N`
  sets the count (`-n 0` = no cap) and `--all` widens to every project.
  `ls --count` still tallies every project.
- `clean` now removes the newest synced session in the current project
  (all of its imported copies, in every target) and keeps the rest of the
  index; `clean --all` keeps the old behavior of removing every import ever
  made. Both accept `--dry-run`.
- `sync --from <tool>` filters the source tools (`--from claude --to pi`); any
  pair works (`--from omp --to codex`, `--from opencode --to pi`, …), and a
  pair whose source is already the target (`--from omp --to omp`) says so
  instead of silently printing a zero row;
  `--from` still accepts session refs, and tool names win when a token is
  both. `sync <tool>` (latest from every other agent into it) is unchanged.
- Commands are split into `src/cmd/` with `src/cli.rs` (flags) and
  `src/scope.rs` (cwd vs `--all`), so the CLI surface lives in one place.


- Imports now mark untransferable content in the transcript: images, redacted
  reasoning, code patches, compaction pointers, and tool calls whose output was
  never recorded each render an in-place `[gap in imported history: …]` marker,
  counted as dropped in fidelity reports.
- Flattened tool calls/results are labeled `transcript, not live` so a resumed
  agent never mistakes past tool output for state it can continue.
- Previously silent drops are now extracted: pi/omp `toolCall` traffic,
  codex `custom_tool_call` traffic, claude `thinking` blocks, and content-bearing
  claude attachments (`file`, `compact_file_reference`, `thinking_stripped`,
  `queued_command`).
- `sync latest` preview reports real dropped counts with gap kinds instead of
  mislabeling every non-text block as dropped.
- Native pickers now show the `[agent]` prefix on imported sessions: pi imports
  carry a `session_info` name record, omp imports set `title` on the session
  record, claude imports write the `custom-title.json` sidecar, and codex
  imports set the `name` column. Each picker's title source was verified
  against its binary. `clean` removes claude sidecars (indexed and orphaned).
- Syncs finish in seconds instead of minutes. Scans reuse cached session
  records per unchanged file, so an actively-growing transcript costs one
  file re-parse rather than a store-wide walk; opencode imports run
  concurrently (bounded by cores) instead of one at a time; and the import
  deadline scales with payload size (opencode's import is ~1.8s/MB, so a
  45MB transcript is no longer killed mid-import).
- Tolerant extraction: a truncated or corrupt JSONL record becomes a visible
  `[gap in imported history: …]` marker instead of failing the whole session,
  and a failed `opencode import` now reports the CLI's own error message.
- Opencode imports fold transcript-context lines into the assistant role,
  which its import validator requires (role `system` was rejected outright).
- `clean` deletes opencode imports in bounded chunks with per-chunk progress.
  A single transaction over hundreds of imports (253k+ rows in a 1.6GB store)
  is SQLite-bound at ~3k rows/s and previously died on a 10s command deadline.

## 0.3.0 — t3code support removed
- `Tool::T3`, the t3 scanner/extractor, picker arm, and docs are gone.
  `sync t3` / `ls t3` now fail as unknown values; pre-existing t3-sourced
  imports were purged from all stores and the index before removal.

## 0.2.0 — consolidated commands

7 subcommands → 4 (`pick` default, `sync`, `ls`, `clean`).

| Old | New |
|---|---|
| `sync` / `sync claude` / `sync pi omp` | unchanged |
| `convert latest --to pi [--dry-run] [--scope N]` | `sync latest --to pi [...]` |
| `convert 01a12ba6 --from omp --to claude` | `sync 01a12ba6 --to claude` (`--from` only if the id collides with a tool name) |
| `convert latest:3 --to codex` | `sync latest:3 --to codex` (`latest:N` now really picks the Nth session) |
| `export latest sharegpt [-o f]` | `sync latest sharegpt [-o f]` |
| `export latest openai` | `sync latest --to openai` |
| `doctor` | `ls --count` |
| `clean --unmarked` | `clean` (sweep always runs) |
| `list …` | `ls …` (`list` kept as hidden alias) |

New: multi-source × multi-target moves (`sync latest 01a12ba6 --to pi claude`);
ref-only `sync latest` previews without writing.

Breaking: `convert`, `export`, `doctor` subcommands removed; `clean` no
longer accepts `--unmarked`; `sync` positionals now mix refs and
destinations (tool/format first, else ref).
