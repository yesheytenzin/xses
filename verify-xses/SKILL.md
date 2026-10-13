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
| Reference validation | `target/debug/xses sync latest:0 --dry-run` | Rejects the invalid reference with a clear error and non-zero exit status. |
| Export disambiguation | `target/debug/xses sync latest sharegpt --out /tmp/xses-export.json` | Multiple selected sessions produce distinct output filenames. |
| Scoped cleanup | `target/debug/xses clean --dry-run` | Reports the newest synced session's artifacts only; other imports stay listed by `ls`. |
| Full cleanup preview | `target/debug/xses clean --all --dry-run` | Reports every import; deletes nothing. |
| Diagnostics | `target/debug/xses doctor` | Prints dependency status, per-store counts, index stats, and journal entries; exits 0. |
| Lock contention | run two syncs at once | The second exits non-zero naming the lock file; a stale lock (dead PID) is removed automatically. |

## Regression gates

Run `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
and `cargo fmt --check` before claiming a change is verified. Inspect live
`ls --all` output separately because existing stores can contain historical
duplicates that unit tests cannot detect.

For import-path changes, exercise a real round trip in a scratch directory
(import a session, confirm the native store lists it under its `[agent]`
prefix, then `xses clean` removes it) because the writers depend on each
agent's shipped formats, not on this repo's tests.
