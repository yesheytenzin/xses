# Transfer capabilities

What a sync carries, where it lands, and what can never move. Statuses:

- **carried** — written as the target's own construct (mapping version 6).
- **text** — preserved as labelled text because the target has no construct.
- **gap** — recorded as `[gap in imported history: …]` and counted as dropped.
- **impossible** — the data is not in the source file or has no equivalent.

## Carried natively

| Data class | claude | codex | pi / omp | opencode |
|---|---|---|---|---|
| text | `text` blocks | `message` (`input_text`/`output_text`) | `text` parts | `text` parts |
| thinking (text) | `thinking` blocks | `reasoning` items | `thinking` parts | `reasoning` parts |
| tool calls | `tool_use` (id, name, input) | `function_call` / `custom_tool_call` | `toolCall` (id, name, arguments) | `tool` part (`callID`, `state.input`) |
| tool results | `tool_result` rows (`tool_use_id`, `is_error`) | `function_call_output` / `custom_tool_call_output` | `toolResult` messages (`toolCallId`, `toolName`, `isError`, `durationMs`) | `tool.state.output` / `state.error` |
| images (≤5 MB each) | `image` base64 blocks | `input_image` data URLs | `image` parts (data + mimeType) | `file` parts (data URL + filename) |
| patches | *text* | *text* | *text* | `patch` parts (hash + files) |
| compaction | *text* | `contextCompaction` projection | `message` text | *text* |
| usage / cost / tokens | per-message `usage` | `token_count` events | per-message `usage` | message `tokens` / `cost` (read side parses its `step-finish` parts) |
| injected context | *text* (`[context: kind]`) | *text* | *text* | *text* |
| per-message time | row `timestamp` | item `timestamp` | row + `message.timestamp` | `time.created` |

Sources are read symmetrically, in each store's own encoding: claude nests an
image as `source.data` + `source.media_type`, codex uses `input_image` data
URLs, pi/omp store `image` parts (inline base64 or a `blob:` reference into
their own blob store, which is resolved), and opencode stores a `file` part
with `mime` + a base64 `url`.

Projections each target's own UI reads:

- claude: `custom-title.json` sidecar (picker row) + uuid parent chain.
- codex: `thread_items` rows — `userMessage`, `agentMessage`, `commandExecution`
  (shell calls, with `aggregatedOutput` so the TUI shows output),
  `fileChange` (patch calls, with per-file kind and diff), `mcpToolCall`
  (server, tool, arguments, status and output), `imageView`, `contextCompaction`.
- pi / omp: `session_info` (picker name), `parentId` chain, `title` record.
- opencode: message parts as stored, `session.slug = xses-<id>`.

## Text fallbacks, by design

- **Tool traffic is history, never live state.** Every import opens with a note
  saying the transcript already ran elsewhere; calls are written as completed,
  and a call whose result the source never recorded becomes a tool-output gap.
- **Thinking signatures are not replayed.** `signature` / `thinkingSignature`
  are provider-bound; the reasoning *text* transfers, the signature does not.
- **Patch bodies** have no home outside opencode, so they arrive as a labelled
  file list.
- **Injected context** (AGENTS.md, environment, reminders) is text everywhere:
  opencode rejects `role: "system"` on import (verified), so it rides as
  assistant text there.
- **Non-shell tools in codex's projection** use codex's own item types where it
  has one (patch calls become `fileChange` with the diff taken from the call's
  arguments, MCP calls `mcpToolCall`, `view_image` an `imageView`); a tool with
  no native item, and any patch whose result failed, renders as a command row.
  The rollout keeps every `function_call`/`function_call_output` either way.
- **Attachments carry what the source embedded**: claude's `file` rows hold the
  text it read (`content.file.content` with its line range) or an image
  payload, and that is what transfers. A bare path still inlines an on-disk
  image within the cap; anything else arrives as a labelled note naming the
  file.

## Not carried, by design

claude rows flagged `isMeta` transfer as context — once, since claude re-sends
the same skill body on every later turn. Two things stay behind: rows flagged
`isSidechain`, and the subagent transcripts claude keeps beside a session
(`<slug>/<session>/subagents/agent-*.jsonl`, 67 in the store checked). A
subagent's conversation is not this transcript, is not listed by claude's own
picker, and cannot be resumed with `claude --resume`, so it is neither a
session to move nor turns to carry.

## Impossible

| Data class | Why |
|---|---|
| Redacted / encrypted reasoning | not readable locally. claude documents `signature` as *"an encrypted copy of the full reasoning"* that its servers decrypt to rebuild the prompt (and calls the field opaque); the decoded envelope is a protobuf of metadata — model, block type, nonces, a 64-byte value — with no plaintext, and 2 048 blocks here carry an empty body against 21 065 readable ones. codex's `encrypted_content` is server-side state meant to be replayed verbatim to the same organization, and no local key exists (`~/.codex/auth.json` holds only tokens). codex's readable counterpart, the `summary` array, does transfer (20 728 rows here) |
| Provider-signed thinking replay | the service verifies provenance and binds the block to model, conversation prefix and account; a re-scoped signature is a 400 (`Invalid signature in thinking block`), and claude's own CLI documents the fallback — it *strips the thinking blocks and retries*. Nothing local can re-sign or re-scope a blob, and xses never imports into the source's own store, so a signature never has a valid destination |
| Live runtime state (running tools, queues, background tasks, permission prompts) | not representable as history in any target; only the import note can describe it |
| Provider-local bookkeeping (latches, bridge ids, file-history snapshots, fork refs, cache keys, credential pins) | same-tool round-trips keep them in the source; other agents have no place for them |
| Non-image attachments with no embedded payload | a claude `file` row that carries only a path has nothing to move: xses inlines on-disk images within the cap, and for anything else writes a note naming the file. Text files claude already embedded (`content.file.content`) do transfer as text |
| Original tool-call ids verbatim | Anthropic validates the *shape* (`^[A-Za-z0-9_-]+$`) and rejects a stray `|` with a 400, and sources compose ids (`call_…|fc_…`). xses therefore maps every id it writes into the target's accepted class, keeping both sides of a pair on the same value; the source id stays in the source |
| Sub-second precision of a message time | stores record milliseconds, but a target that stores whole seconds (where its own schema does) keeps the second; ordering and the date are exact |

## Sessions that are not worth resuming

Three shapes are treated as noise and hidden by default (they carry no work to
continue): `empty` (no user or assistant turns at all), `unanswered` (the user
asked and no assistant turn ever arrived), and `failed` (assistant turns that
are only error/abort markers). Detection prefers each store's own signal —
claude `isApiErrorMessage`, pi/omp `stopReason` in error/aborted/signal, codex
`turn_aborted`, opencode message `error` — falling back to error-shaped text
(`API Error: …`, `rate limit`, `stream error`, …).

They are never *deleted* and never silent: `ls --count` and `doctor` report the
tally per reason, `ls --include-noise` lists them with a `!reason` marker, and
`pick`/`sync` accept `--include-noise` when they are wanted.

## Naming and updates

Re-sync updates in place: a source session's artifact path (and opencode id)
derives from its id, and a changed source rewrites that one artifact — the
index keeps a single record per (target, source) pair, so pickers never grow a
second row for the same session. Row titles come from the source agent's own
title when it records one (pi/omp `title`, opencode `session.title`, or the
title an earlier import carried), falling back to the first user ask; the
first-ask fallback is why two *different* sessions that open with the same
message can still look alike in a list.

## Accounting

Every import header carries a manifest — `xses: {source, srcId, mapping,
blocks: {kind: count}, gaps: [kinds]}` — so the file itself states what it
holds and what it lost. `sync --dry-run` prints the same tally before writing,
and `xses doctor` aggregates recorded failures. A future mapping bump
re-imports old copies automatically (the mapping version is part of the
idempotency fingerprint).

## Verified against the live CLIs

- `claude --resume` and `pi -r` list and open imports; `omp -r` lists them.
- codex accepts the drop-in rollout and renders `commandExecution` rows.
- `opencode import` accepts `tool`, `reasoning`, `patch`, **and `file` parts**
  (data URL, mime, filename — the shape its own `-f/--file` flag builds).
  It requires `state.output` **and** `state.error` on every tool part, rejects
  `role: "system"`, and opens SQLite with `busy_timeout=0`, so xses imports are
  serialized for that store: two writers never wait for each other, the loser
  fails outright.
