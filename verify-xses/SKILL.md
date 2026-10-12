# Verifying xses

## Boot

Build with `cargo build`. The CLI is ready when `target/debug/xses --help`
prints the command list. No daemon or external service is required, but live
OpenCode checks require the local `opencode` binary and database.

## Features and pass criteria

| Feature | Flow | Pass criteria |
|---|---|---|
| List | `target/debug/xses ls --all` | Every row starts with its owning agent label, such as `[claude] title`; command exits successfully. |
| Counts | `target/debug/xses ls --count` | Per-agent counts print and the command exits successfully. |
| Dry-run sync | `target/debug/xses sync --dry-run` | Reports planned writes/losses without changing the xses index or target stores. |
| Reference validation | `target/debug/xses sync latest:0 --dry-run` | Rejects the invalid reference with a clear error and non-zero exit status. |
| Export disambiguation | `target/debug/xses sync latest sharegpt --out /tmp/xses-export.json` | Multiple selected sessions produce distinct output filenames rather than overwriting one another. |
| Cleanup preview | `target/debug/xses clean --dry-run` | Reports removable imports without deleting native session files. |

## Regression gates

Run `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
and `cargo fmt --check` before claiming a change is verified. Inspect live
`ls --all` output separately because existing stores can contain historical
duplicates that unit tests cannot detect.
