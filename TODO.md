# TODO — Technical Debt Paydown (ranked by interest)

- [x] 1. Split `write/codex.rs` (was 1139 lines) → `write/codex/{mod,rollout,roller,tools,seed}.rs`
  (134/204/439/139/262). Done: tests + clippy `-D warnings` + fmt green.
- [x] 2. Central SQL choke point (`proc::sql_quote` / `sql_quote_id` / `sql_id_list`).
  The `sqlite3` CLI has no bind parameters, so ids are allowlisted and refused
  instead of escaped; free text still quotes by doubling. Converted: opencode
  bulk delete, codex seed/register/remove/retire, opencode snapshot ids, scan
  cwd filter. Done: hostile-id unit test (`' OR '1'='1`) green.
- [x] 3. Matrix coverage (`src/matrix.rs`, test-only): per-pair import-id
  determinism + pairwise distinctness across all 5 tools, per-target
  fingerprints (mapping/scope/revision participation), failure-mode contracts
  (hostile ids rejected, empty clean is a no-op). 80 tests green.
  Remaining (explicit next step): hermetic fixture-store e2e for `--from X --to Y`
  plus live failure modes (locked DB, missing binaries, partial import + retry).
- [x] 4. One owner for opencode generations (`scan.rs`): `delete_opencode_sessions`
  (moved out of `sync.rs`) + `opencode_session_union(id, legacy)` serving both
  snapshot and legacy-digest reads. `session_v2` SQL now only in `scan.rs`
  (extract keeps row parsing only).
- [x] 5. Retry without the global wait (`write/opencode.rs`): first attempts run
  in parallel; backoff (named `IMPORT_ATTEMPTS`/`RETRY_BASE_MS` + deterministic
  jitter) sleeps *outside* the gate, which is held only around a retried import.
- [x] 6. No production `.expect()` on IO: `proc.rs` pipe takes, `refs.rs`
  candidate pick, `cmd/mod.rs` resume argv, `sync.rs` report merge all return
  errors. Remaining `.expect(` are test-only (verified by audit).
- [x] 7. Fallback audit: every `unwrap_or_default`/`unwrap_or(Null/0/png)` on the
  write path carries an `// Intentional fallback:` comment stating why the
  default is correct; suspicious ids journal via `codex-seed`/`codex-register`.
- [x] 8. One mime default: `write::DEFAULT_IMAGE_MIME` + `image_mime()` used by
  all 5 writers (only remaining `"image/png"` literals are the const itself,
  canonical test fixtures, and an unrelated extractor map).
- [x] 9. Doctor de-stringed: `format_issue_line()` is pure and pinned by
  fixture-shape tests (locked-DB / schema-error / missing-CLI); production
  never matches on upstream wording (display path only strips + shortens).
- [x] 10. Named consts: `OPENCODE_DELETE_CHUNK` (now with `scan.rs`), retry
  policy consts, `FIXTURE_EPOCH_SECS`, `fixture_created()` anchor shared by
  claude/doctor tests.

- [x] 11. opencode note-id collision (live bug, found via `xses sync` 500s):
  every import carried the constant message id `msg_000000-note`; v2
  `session_message.id` is globally unique, so the second-ever opencode
  import failed with `Internal Server Error` on PK conflict (bisected live:
  only that exact id failed). Fixed with a per-session note id
  (`msg_<ses-hash>-note`); `XSES_KEEP_PAYLOAD=1` hatch added to the writer
  for capturing payloads for manual `opencode session import` replays.

- [x] 12. codex result-side ids bypassed `TargetIds` (live bug, exposed by
  the item-2 allowlist): `push_tool_result` used the raw source call id
  while the call side mapped it, so `call_x|fc_y` pairs never met and the
  raw `|` tripped the SQL allowlist at seed time. Both sides now share the
  mapped id (pending lookup compares like with like). MAPPING 8 → 9 so old
  raw-id copies upgrade; regression test
  `composite_call_ids_meet_on_the_mapped_value` green.

- [x] 13. omp imports were unmarked + a record pointed at a native file
  (live incident): the omp writer mimics native record shapes (validated
  fields), so copies carried no `xses` marker — invisible to echo
  protection and clean — and a same-millisecond native filename collided
  with an import path, leaving an index record aimed at a live native
  session (one `clean` away from deleting it). Fixed in depth:
  `*.xses.json` provenance sidecars (mirroring claude) written + recorded
  + read by scan/sweep/provenance; writers refuse to overwrite unmarked
  files (`target_is_ours` clobber guard); `clean` and `retire_artifacts`
  refuse unmarked files instead of deleting; `doctor` reports `unmarked`
  records. MAPPING 10 → 11. Regression tests: sidecar round-trip,
  clobber refusal (file byte-identical), `removable_import` predicate,
  index keeps records whose retirement is refused.
  Also fixed: test journal pollution via `XDG_DATA_HOME` (it overrides
  `$HOME` for `dirs::data_dir`).

- [x] 14. opencode delete/read race vs live server (live incident): with
  stable ses ids, every live update hits `already exists` → delete →
  re-import — but `opencode serve` answers reads from a stale snapshot, so
  the re-import conflicted while the row was already gone, and blind
  re-deletes then destroyed the copy (2546-message session lost, restored
  via re-sync). Fixed: delete once, settle-poll `session export` (bounded
  20 × 500ms) until the server agrees the id is gone, never delete twice;
  a repeat conflict waits instead. Plus `XSES_ISSUES_FILE` journal
  override so unit tests can't pollute the real journal (wired into pi,
  claude, and index test setups; verified zero journal growth on test).
