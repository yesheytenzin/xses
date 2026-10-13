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
| images (≤5 MB each, 20 MB/session) | `image` base64 blocks | `input_image` data URLs | `image` parts (data + mimeType) | *gap* — no importable image part |
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
- `opencode import` accepts `tool`, `reasoning`, and `patch` parts, and requires
  `state.output` **and** `state.error` on every tool part — a call with no
  recorded output carries an empty one rather than failing the import.
