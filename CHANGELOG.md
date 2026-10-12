# Changelog

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
