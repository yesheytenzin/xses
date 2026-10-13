# Transfer capabilities

What a sync between agents can carry, what it cannot, and why.
Three statuses are used throughout:

- **shipped** — done today.
- **possible** — reachable by the normalize-and-project design (`docs`/plan): the
  source has the data and the target has a construct for it; a writer mapping is
  all that is missing. Items marked *(probe)* still need their target validator
  confirmed against the live CLI.
- **impossible** — the data is not in the source file, is provider-bound, or has
  no representable equivalent.

## Shipped today

Sync is text-only. Every structured block is flattened into a labelled text
part, with per-block fidelity counted and visible to the user:

| Source block | Becomes | Fidelity |
|---|---|---|
| text | text | preserved |
| thinking | `[previous thinking — transcript, not live]` + text | downgraded |
| tool call | `[previous tool call: NAME — transcript, not live]` + args | normalized |
| tool result | `[previous tool result: NAME — transcript, not live]` + output | normalized |
| compaction | `[compaction]` + summary | normalized |
| image, patch, attachment, redacted reasoning, unreadable record | `[gap in imported history: …]` | dropped |

`sync --dry-run` prints the tally; every import also reports what it lost.
Imports are idempotent (fingerprint per session content) and `clean` removes
exactly what xses wrote.

## Possible (content transfer, not yet implemented)

| Data class | Verdict | Target construct | Caveats |
|---|---|---|---|
| Text | possible → **native everywhere** | all five formats | — |
| Tool calls (name, args, status) | possible | claude `tool_use`, codex `function_call` / `custom_tool_call`, pi `toolCall`, opencode `tool` part *(probe)* | call ids are re-minted per target; ids are provider-bound |
| Tool outputs (incl. `is_error`, duration) | possible | claude `tool_result`, codex `function_call_output`, pi `toolResult` message, opencode `tool.state.output` *(probe)* | re-parenting differs per target: claude puts results in a user row, codex as its own item, pi as its own message, opencode folds them into the assistant part |
| Thinking text | possible (text only) | claude `thinking`, codex `reasoning`, pi `thinking`, opencode `reasoning` part | signature-bound: the provider validates signatures, so the block is never "live reasoning" again |
| Injected context (AGENTS.md, reminders, environment) | possible, content only | claude system rows, codex developer/system items, pi `custom_message` | opencode's import validator rejects `role:"system"` (measured) → folded into assistant text |
| Compaction summaries | possible | pi/opencode `compaction`, claude `compact_boundary` *(probe)*, codex summary message | recorded as history; the target's own context window is unaffected |
| Images / screenshots | possible with caps | claude `image` base64, codex `input_image` data URL, pi `image{data,mimeType}` | caps 5 MB per part / 20 MB per session; opencode `file` part *(probe)*; over-cap → gap |
| Patches / diffs | text elsewhere | opencode `patch` part *(probe)* | other agents have no patch construct |
| Usage, cost, tokens, per-message model | possible | opencode `step-finish`, pi message fields, codex `token_count` events, claude usage rows | accounting models differ; the numbers are metadata, not authoritative |
| Session metadata (title, cwd, git branch, permission mode, model) | possible | claude session rows, codex `session_meta`/`turn_context`, pi header + `session_info`, opencode session columns | fields with no home are preserved in `raw_extra` |
| Subagents / sidechains | possible as separate sessions | — | transcripts transfer; the nesting relation has no cross-agent equivalent |

## Impossible

| Data class | Why |
|---|---|
| Redacted / encrypted reasoning | the content is not in the source file. Claude stores empty `thinking` with a signature (99% of observed blocks: 561 of 566 in one sample, 1471 of 1476 in another); every observed codex `reasoning` item had null content (89 of 89). Only a gap marker is honest |
| Provider-signed thinking replay | `signature` / `thinkingSignature` are provider-validated; emitting them elsewhere is rejected or meaningless |
| Live runtime state (running tools, queues, background tasks, permission prompts) | not representable as history in any target; only a narration note can be written |
| Provider-local bookkeeping (latches, bridge ids, file-history snapshots, fork refs, cache keys, credential pins) | same-tool round-trips keep them in `raw_extra`; no other agent has a place for them |
| Attachment bytes that no longer exist | claude `file` / `compact_file_reference` carry a path, not content; if the file is gone there is nothing to move |
| Exact per-message timing | some sources have no usable timestamps; ordering is preserved, timing is synthesized where the target requires monotonic values |
| Original tool-call ids | ids are validated per provider; they are re-minted |

## Per-target matrix

| Part | claude | codex | pi / omp | opencode | sharegpt / openai |
|---|---|---|---|---|---|
| text | yes | yes | yes | yes | yes |
| thinking (text) | possible | possible | possible | possible *(probe)* | text |
| tool call | possible | possible | possible | possible *(probe)* | text |
| tool result | possible | possible | possible | possible *(probe)* | text |
| image | possible (capped) | possible (capped) | possible (capped) | *(probe)* | no |
| patch | text | text | text | possible *(probe)* | text |
| injected context | possible | possible | possible | text only (no system role) | text |
| compaction | possible *(probe)* | text | possible | possible *(probe)* | text |
| usage / cost | possible | possible | possible | possible | no |
| session metadata | partial | partial | partial | yes | no |

*(probe)* = the target's own validator has not been run against that construct
yet; the planned probe step fills this in before any writer relies on it.

## Rules that apply to every transfer

1. **History, never live state.** Tool traffic is written as completed history
   with a narration note; a call without its result becomes a gap, never a
   pending call.
2. **Grouping is target-native.** One source message maps to one target message
   where the schema allows multiple parts (claude content blocks, pi parts,
   opencode parts); codex splits calls into their own items because that is how
   its format expresses them.
3. **Every absence is recorded.** Unmappable parts become named gaps, and each
   import carries a manifest of what it could not carry, which `xses doctor`
   aggregates.
4. **Deterministic bytes.** The same source and mapping version produce
   identical artifacts, so re-syncs dedupe and `clean` stays exact.
5. **Caps are explicit.** Size caps for inline media and the mapping version
   live in the manifest, so a future release can raise them and re-import.
