# Transfer capabilities

What a sync carries, where it lands, and what can never move. Statuses:

- **carried** — written as the target's own construct (mapping version 2).
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
| images (≤5 MB each, 20 MB/session) | `image` base64 blocks | `input_image` data URLs | `image` parts (data + mimeType) | `file` parts (data URL + filename) |
| patches | *text* | *text* | *text* | `patch` parts (hash + files) |
| compaction | *text* | `contextCompaction` projection | `message` text | *text* |
| usage / cost / tokens | per-message `usage` | `token_count` events | per-message `usage` | `step-finish` tokens |
| injected context | *text* (`[context: kind]`) | *text* | *text* | *text* |

Projections each target's own UI reads:

- claude: `custom-title.json` sidecar (picker row) + uuid parent chain.
- codex: `thread_items` rows — `userMessage`, `agentMessage`, `commandExecution`
  (shell calls, with `aggregatedOutput` so the TUI shows output), `contextCompaction`.
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
- **Non-shell tools in codex's projection** render as text; the rollout still
  holds their `function_call`/`function_call_output` items.

## Impossible

| Data class | Why |
|---|---|
| Redacted / encrypted reasoning | not in the source file (claude stores empty `thinking` with a signature; codex stores `reasoning` with null content) |
| Provider-signed thinking replay | signatures are validated by the originating provider |
| Live runtime state (running tools, queues, background tasks, permission prompts) | not representable as history in any target; only the import note can describe it |
| Provider-local bookkeeping (latches, bridge ids, file-history snapshots, fork refs, cache keys, credential pins) | same-tool round-trips keep them in the source; other agents have no place for them |
| Attachment bytes that no longer exist | claude `file` / `compact_file_reference` carry a path; if the file is gone there is nothing to move |
| Original tool-call ids | ids are provider-validated; xses re-mints deterministic ids so results still pair |
| Exact per-message timing | ordering is preserved; timestamps are synthesized where the target requires monotonic values |

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
