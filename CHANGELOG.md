# Changelog

## 0.8.0

- Sync continued imported sessions back as separate branches while preserving
  originals; ignore untouched copies to prevent sync loops. Continuation
  detection includes assistant, image, and tool activity.
- Preserve changed imports and imports with missing or invalid baselines during
  replacement and cleanup. `clean --force` explicitly removes protected copies.
- Recover interrupted imports with durable undo journals, restoring native files
  and database records. Hold the operation lock across discovery and writes.
- Repair missing artifacts and native registrations, validate provenance before
  cleanup, and preserve healthy progress when individual sessions fail.
- Cache scoped file scans and OpenCode database counts, invalidate on database
  and WAL changes, and bound snapshot batches and child-process output.
- Support OpenCode v1-only, v2-only, and mixed databases, preferring v2 records
  when IDs overlap. Preserve agent title prefixes and handle import conflicts.
- Resume picker selections in their original directory and propagate native
  agent failures. Fix Codex projected transcripts and Pi import routing.
- Mapping 14 refreshes imports; older copies without trustworthy continuation
  baselines are preserved. Pause native target work while replacing imports.
- Add cross-platform CI and CLI safety checks. Fix release checksum filenames
  and preserve every platform binary when generating build attestations.

### Additional fixes since 0.7.2

- Tech-debt paydown (TODO.md, all 10 items): split `write/codex.rs` into
  `write/codex/{mod,rollout,roller,tools,seed}.rs`; central SQLite quoting
  (`proc::sql_quote`/`sql_quote_id`/`sql_id_list`) with hostile ids refused,
  applied to opencode deletes, codex seeding/registration/retirement, and
  opencode snapshot reads; opencode v1/v2 SQL owned by `scan.rs`
  (`delete_opencode_sessions`, `opencode_session_union`); opencode retries
  back off with jitter outside the serial gate; no production `.expect()` on
  IO; documented silent fallbacks; one `DEFAULT_IMAGE_MIME`; pure
  `doctor::format_issue_line` with fixture tests; named retry/fixture consts.
  Test count 75 → 80 (`matrix` id/fingerprint/failure-mode coverage).
- Fixed every opencode import after the first failing with `Internal Server
  Error`: the import-note message reused the constant id `msg_000000-note`
  while v2 message ids are globally unique. The note id is now per-session,
  and `XSES_KEEP_PAYLOAD=1` keeps the import payload for manual replays.
- Fixed codex imports of sessions with composite call ids (`call_x|fc_y`):
  result rows now share the call side's mapped id instead of carrying the
  raw id (which also tripped the new SQL allowlist). MAPPING 8 → 9.
- Omp/pi imports now carry `*.xses.json` provenance sidecars (recorded,
  swept, and read by scan); writers refuse to overwrite unmarked files,
  and `clean`/retirement refuse to delete them — a record can no longer
  aim at a native session. `doctor` reports `unmarked` records.
  MAPPING 9 → 10 (stable artifact identity) → 11 (sidecars).
- Opencode updates now settle after delete (`session export` poll, bounded)
  instead of racing the live server's snapshot; repeat conflicts wait,
  never re-delete. Test journal sandboxed via `XSES_ISSUES_FILE`.

## 0.7.2

- Removed the push-CI workflow; the release workflow keeps its own test
gate, so every tag is still gated on tests and clippy.

## 0.7.1

- Pre-snapshot opencode digest keys migrate without re-importing: a
  fingerprint miss that hits the legacy digest records the new key over
  the same artifacts. Legacy digest queries run only on misses and taper
  to zero as sources migrate.
- opencode 1.x installs get a clear error at write time (imports need
  opencode 2+, or xses 0.6.0 for 1.x stores) instead of failing inside
  the v2 schema validator.

## 0.7.0

- opencode v2 stores are fully supported alongside v1: `session_v2` +
  `session_message` are scanned and extracted (v1 `session`/`message`/`part`
  keep working), and writes go through `opencode session import` with the
  v2 export shape (flat messages, inline content, `files[]` images) and
  `metadata.xses` provenance.
- Mapping 8: all imports refresh once; old generations retire automatically.
- Note: imports now target the opencode v2 format; opencode 1.x installs
  can still be read, but no longer receive imports.
- opencode reads are down to one SQLite query per session: the revision
  UNION feeds both the content digest and the transcript parse (previously
  the digest plus one query per table, plus a generation probe). Revision
  digests for bulk syncs run in parallel.
- The digest input gained `message_id` on part rows, so opencode sources
  re-sync once; retirement replaces the old generations automatically.
- v1 opencode transcripts keep their per-message times (the union carries
  each row's `created`; the old per-table query never recorded them).
- All SQLite access goes through shared `proc::sqlite_*` helpers with
  timeouts; scans no longer hang on a wedged store.
- Internal splits with no output change: codex `rollout_items` → per-block
  methods, opencode `write()` → message builders, `sync()` → phases.

## 0.6.0 — mapping 6: tool ids, message times and restored outputs

The transfer layer stopped assuming what a store *might* hold and started
recording what it does: ids that the target's own validator accepts, the time
each source message actually happened, and the tool outputs claude keeps beside
a transcript instead of in it. Around that, the picker, `doctor` and the index
lock grew the guard rails a daily driver needs.

Imports carry the source's own structure instead of flattening it to text: one
source message maps to one native message, and its parts map to the target's
constructs:

- tool calls and results travel as `tool_use`/`tool_result` (claude),
  `function_call`/`function_call_output` (codex), `toolCall`/`toolResult`
  (pi/omp), `tool` parts (opencode) — with correlation ids, error state, and
  durations, so a resumed agent sees what ran and what it returned.
- reasoning travels as `thinking` / `reasoning` parts (text only: provider
  signatures are never replayed).
- images travel inline (base64/`input_image`/`image` parts) up to 5 MB per
  part; larger ones become named gaps.
- codex projections carry `commandExecution` rows (with output) and usage
  arrives as `token_count` events; opencode gets `reasoning`, `tool`, and
  `patch` parts plus `step-finish` tokens; claude gets per-message `usage`.
- every import opens with a note that the transcript already ran elsewhere, so
  tool traffic is read as history, never continued as pending state.
- images also reach opencode, as `file` parts carrying the data URL (verified
  against the live importer; its schema is `type/mime/filename/url`).
- images kept as content-addressed references (`blob:sha256:<hash>`) are
  resolved from the agent's own blob store (both pi's and omp's stores are
  searched) and inlined as base64 instead of arriving as a broken data URL.
  A mapping bump re-imports existing copies once so old imports stop carrying
  the unresolved form.
- `--from <tool>` scopes the candidate stores before refs resolve, so
  `sync --from pi <id>` picks the pi session even when the same id also exists
  in stores it was imported into.
- opencode imports are serialized: its CLI opens SQLite with `busy_timeout=0`,
  so a second writer fails instead of waiting. Retries back off three times for
  the case where another app is writing.
- `sync <id>` no longer reports "ambiguous" just because the id also exists in
  stores it was imported into: the original session wins, then the newest.
- imports are titled with the source agent's own session title when it has one
  (pi/omp `title`, opencode `session.title`, a previous import's title), and
  only fall back to the first ask when the agent recorded none. Re-syncing
  already updated a session in place — a source that changes rewrites its one
  artifact — but titles built from the first ask made distinct sessions with
  the same opening line look like duplicates in the other agents' pickers.
- import headers carry a manifest (`blocks`, `gaps`, `mapping`), the mapping
  version is part of the idempotency fingerprint (older copies re-import once),
  and `FEATURE.md` documents what is carried, text-only, or impossible.

- the remaining impossible rows are now evidence-backed rather than assumed:
  claude documents its `signature` as an encrypted copy of the full reasoning
  that only its servers decrypt (the decoded envelope here is protobuf metadata
  with no plaintext), and its CLI documents that a rejected signature makes it
  strip thinking and retry; codex's
  `encrypted_content` is server-side encrypted state replayed verbatim to the
  same organization, with no local key (`~/.codex/auth.json` holds only tokens)
  and only its readable `summary` transfers. Both were checked with REA against
  the shipped codex binary and claude's bundled CLI, plus vendor docs.
- tool ids are mapped into the target's accepted class. Anthropic validates
  `tool_use.id` against `^[A-Za-z0-9_-]+$` and answers a stray character with a
  400, while sources compose ids (`call_…|fc_…` from codex/omp): 108 imported
  claude transcripts here carried 36 011 such ids, so continuing them would
  have failed at the API. Both sides of a pair now share one mapped value, and
  the mapping is applied by every writer so a target never depends on another
  provider's id space.
- every message keeps the time its source recorded: the IR carries the time per
  source message and the writers emit it (claude row `timestamp`, codex item
  `timestamp`, pi/omp row + `message.timestamp`, opencode `time.created`),
  stepping forward a millisecond only when a target needs strictly increasing
  values. Previously every imported row carried the session's creation time.
  Mapping version 6 re-imports existing copies once so old rows pick this up.
- claude's truncated tool outputs are restored: when a transcript row only
  holds a preview plus a `toolUseResult.persistedOutputPath`, the full text is
  read from claude's `tool-results/` sidecar (10 rows, 0.72 MB here) and
  carried as a labelled `[context: tool-output]` block. Context rather than a
  tool result: the row is a plain user string that claude never paired with a
  call id, and claude's own resume path rejects a result whose id matches no
  call.
- sessions that were never useful are hidden from `ls`, `pick` and `sync`:
  `empty` (no turns), `unanswered` (nothing ever answered the user) and
  `failed` (assistant turns that are only API/abort errors), each detected from
  the store's own signal where it has one. `ls --count` reports the tally,
  `--include-noise` lists or acts on them, and listed rows carry a `!reason`.
- `doctor` reports only what is installed: stores of missing agents are not
  listed, each store is read on its own so an unreadable one reports itself
  (`opencode  unavailable: … sqlite3 is not installed`), and every issue is a
  single line — control sequences stripped, clipped to the terminal width, with
  the journal path printed when something was clipped.
- `sync` and `clean` draw a one-line progress bar on stderr while they work
  (phase, counts, elapsed, failures, the session being written), shown only on
  a terminal and silenced by `XSES_NO_PROGRESS=1`.
- the index lock is enforced by identity: mutations refuse to write once the
  lock file is gone or replaced, the guard re-acquires it atomically when it
  vanished and nobody else took it, and a run never deletes a lock it no longer
  owns.
- claude images carry their bytes and media type (the payload is nested under
  `source`, and its `media_type` was ignored, so images arrived as text
  placeholders and a WebP/JPEG would have been re-labelled png).
- opencode `file` parts that hold images are read as images; they are how a
  live store keeps an attached picture.
- claude attachments carry their payload: embedded text (with the line range it
  covers) and image payloads transfer, a bare path inlines an on-disk image
  within the cap, and anything else is a note naming the file. Compaction
  references keep embedded content too.
- codex `compacted` rows are carried as compaction instead of being ignored,
  and claude's `isMeta` rows (skill bodies, local-command output) transfer once
  as context — claude re-sends the same injection on every later turn.
- imported tools keep codex's own item types in the projection: patch calls
  become `fileChange` with per-file kind and diff, `mcp__server__tool` calls
  `mcpToolCall` with arguments, status and output, `view_image` an `imageView`;
  a failed patch still shows a row so its error is visible.
- writer failures journal `write-failed` with the path and the OS error instead
  of a bare "target wrote nothing".
- claude's subagent transcripts (`<slug>/<session>/subagents/agent-*.jsonl`) are
  not sessions: claude's picker never lists one and `claude --resume` cannot
  open it, so they are skipped like the `isSidechain` rows they belong to.

Also fixed along the way: `xses ls | head` exits quietly (`SIGPIPE`'s default
disposition is restored), and the `xses doctor` report no longer panics when
its output is piped.

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
