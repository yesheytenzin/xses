# Verifying xses

## Boot

Build with `cargo build`. The CLI is ready when `target/debug/xses --help`
prints the command list. No daemon or external service is required, but live
OpenCode checks need the local `opencode` binary, its SQLite store, and
`sqlite3` on `PATH`.

## Features and pass criteria

| Feature | Flow | Pass criteria |
|---|---|---|
| List (capped) | `target/debug/xses ls` | At most 5 rows and exits 0; every row starts with its owning agent label (`[claude] title`). |
| List (all) | `target/debug/xses ls --all` | More rows than the capped list; exits 0. |
| List (counts) | `target/debug/xses ls --count` | Per-agent tallies print; exits 0. |
| Source filter | `target/debug/xses sync --from claude --to pi -n 3 --dry-run` | One target, sources limited to claude; exits 0. |
| Same-tool no-op | `target/debug/xses sync --from omp --to omp -n 1 --dry-run` | Reports 0 written **and** prints the "already one of the targets" note. |
| Dry-run sync | `target/debug/xses sync --dry-run` | Reports planned writes/losses without changing the index or target stores. |
| Continued imports | `python3 verify-xses/flows/continuations.py target/debug/xses` | Newly continued imports return as branches; untouched copies never echo; repeated sync is idempotent; refreshes preserve changed copies. |
| Reference validation | `target/debug/xses sync latest:0 --dry-run` | Rejects the invalid reference with a clear error and non-zero exit status. |
| Export disambiguation | `target/debug/xses sync latest sharegpt --out /tmp/xses-export.json` | Multiple selected sessions produce distinct output filenames. |
| Scoped cleanup | `target/debug/xses clean --dry-run` | Reports the newest synced session's artifacts only; other imports stay listed by `ls`. |
| Full cleanup preview | `target/debug/xses clean --all --dry-run` | Reports removable imports and preserved changed/unknown copies; deletes nothing. |
| Forced cleanup | `target/debug/xses clean --all --force --dry-run` | Explicitly includes continued and unknown copies in the deletion preview. |
| Import safety | `python3 verify-xses/flows/safety.py target/debug/xses` | Previous history survives failed refresh; missing artifacts recover; cleanup preserves local work and retires OMP sidecars reliably. |
| Diagnostics | `target/debug/xses doctor` | Prints dependency status, per-store counts, index stats, and journal entries; exits 0. |
| Lock contention | run two syncs at once | The second exits non-zero naming the lock file; OS ownership releases on process exit or crash. The lock file persists and must not be deleted. |

## Regression gates

Run `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
and `cargo fmt --check` before claiming a change is verified. Inspect live
`ls --all` output separately because existing stores can contain historical
duplicates that unit tests cannot detect.

For import-path changes, exercise a real round trip in a scratch directory
(import a session, confirm the native store lists it under its `[agent]`
prefix, then `xses clean` removes it) because the writers depend on each
agent's shipped formats, not on this repo's tests.

## Automated CLI regression flows

Run `python3 verify-xses/flows/regression.py target/debug/xses` on Linux after
building. It creates disposable native-format source files and a real SQLite
fixture, then drives the actual CLI through filtered listing, cache reuse,
scoped/full re-sync, idempotency, failed replacement, concurrent lock ownership,
OpenCode same-length edits/deletions, scoped cleanup, and full cleanup. It
prints the path of a JSON transcript containing every command and raw output.
It does not invoke real agent imports or make model requests.

`python3 verify-xses/flows/continuations.py target/debug/xses` drives 40 CLI
commands through disposable Claude, Codex, Pi, and oh-my-pi stores. It imports
with the real xses writers, appends native user/assistant records to simulate
continuing each copy, and checks dry-run, repeated prompts, branch updates,
metadata-only changes, and conflicts on both sides. Codex uses disposable
SQLite registration/projection tables. It makes no model requests and does
not prove native agent resume behavior or OpenCode server imports.

Run `python3 verify-xses/flows/safety.py target/debug/xses` for rollback,
missing-artifact repair, missing/corrupt baselines, partial progress, repeated
OMP cleanup, assistant/image continuations, and interrupted-publication recovery.
Run `python3 verify-xses/flows/native.py target/debug/xses` when local CLIs and
initialized Codex schemas are available: it clones only schema/migration data,
isolates OpenCode with standalone servers when supported, and checks native
Pi export, Codex loading, and OpenCode replacement/cleanup without model calls.
The native flow also kills a Codex publication between file/database writes,
verifies exact recovery, and injects an OpenCode replacement failure to prove
the previous native export is restored.

Run `python3 verify-xses/flows/benchmark.py` after a release build for real-store
listing timings with isolated xses caches. `--baseline /path/to/old/xses` compares
binaries on the same stores. These are application-cache, not cold-disk timings.

## Native transcript UI

Run `python3 verify-xses/flows/native.py target/debug/xses --ui` with tmux and
all five agent CLIs installed. It uses disposable stores and no model requests.
Pass criteria: native tool calls/results render, skill reads fold by default,
Codex dates use realistic epoch seconds, and OpenCode has no bogus empty rows.
The flow also converts a hidden Claude skill injection into a Pi tool result
without losing its historical instructions. Captured UI text is saved beside
the native flow's command log.

## Pi imported compaction summaries

Pi imports store standalone compaction summaries as native `compactionSummary`
messages. Pi folds them when resuming and keeps every summary in model context.
The native Pi HTML exporter embeds these messages in its session data, but its
conversation view currently omits this message role. Summary text remains
available to xses exports and round trips.
