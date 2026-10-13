# done

From a project directory (all scoped to cwd unless noted):

- [x] `xses sync` — sync the latest session between agents (newest 1 per agent,
      every other agent as target; `-n N` for more)
- [x] `xses ls` — show top 5 latest (`-n N`, `-n 0` = no cap)
- [x] `xses pick` — shows top 5 latest (same flags; Enter resumes)
- [x] `xses clean` — cleans the latest session synced (all of its copies, in
      every target; other imports and their index records survive)
- [x] `xses sync --from claude --to pi` — sync claude sessions into pi
      (any tool pair; tool names filter sources, refs still work)
- [x] `xses sync claude` — sync the latest session from every other agent into
      claude
- [x] `xses --help` — help
- [x] `xses --version` — version
- [x] `xses doctor` — shows what failed or any issue from any operation
      (missing CLIs, store counts, index health, journaled failures)

From anywhere (`--all`):

- [x] `xses sync --all` — syncs all sessions from every project between every
      agent
- [x] `xses pick --all` — shows all sessions to pick from
- [x] `xses ls --all` — shows all sessions
- [x] `xses clean --all` — cleans every session ever imported
