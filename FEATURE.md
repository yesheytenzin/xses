# Transcript transfer

xses imports recorded history into native agent stores. It does not transfer
running processes, provider credentials, live tool state, or replayable signed
reasoning. Each import starts with a history note.

| Data | Transfer behavior |
|---|---|
| User and assistant text | Preserved in native message records. |
| Tool calls and results | Converted to each target's tool vocabulary; IDs are mapped consistently. |
| Reasoning | Plain text is carried; provider signatures are not replayable. |
| Images | Supported inline payloads are carried within the extractor's size budget; unavailable or oversized images become visible gaps. |
| Patches | Kept as native file changes where supported; otherwise represented as recorded context. |
| Compaction | Summaries are carried explicitly as compressed history. |
| Token and cost accounting | Recorded values are carried where the target format supports them; they are historical accounting. |
| Unreadable or missing data | Represented as visible `[gap in imported history: …]` markers. |
| Titles | Retain the original `[agent]` provenance prefix. |

Use `xses sync SESSION_ID` to preview a transcript and its fidelity totals.
The command is a preview when no destination is supplied.

## Continuations and safety

New imports record native message IDs and semantic block fingerprints. User,
assistant, image, and tool additions make an imported copy eligible for ordinary
sync. Compaction and title changes alone do not create a new branch. Repeated
syncs update the same return branch; they do not merge into the original native
session.

Before replacement or cleanup, the saved baseline must prove that the target
history has not changed. Missing, older, corrupt, or changed baselines preserve
the existing copy. Explicitly sync a session by ID to share an older copy.
`xses clean --force` also removes continued and unknown copies.

Imports journal before-images of affected native files and Codex rows; OpenCode
replacement saves a validated native export before deleting an existing copy.
An interrupted publication is recovered by the next mutating sync or clean.
Native agents do not share xses's lock: pause work in a target while replacing or
cleaning it to avoid simultaneous writes.

## Verification

`verify-xses/flows/regression.py`, `continuations.py`, and `safety.py` drive the
real xses CLI against disposable stores. `native.py` additionally checks native
Pi export, Codex thread loading, and OpenCode imports without model requests.
See [the verification checklist](verify-xses/SKILL.md).
