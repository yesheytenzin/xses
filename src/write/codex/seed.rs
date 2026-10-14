//! codex store registration: `threads` rows + history projections.
//!
//! Drop-in rollouts are invisible to codex's picker and TUI until their
//! metadata rows exist; seeding mirrors native row shapes (verified by diff).
use super::Item;
use crate::canonical::CanonicalSession;
use std::fs;
use std::path::PathBuf;

/// Quote one id or bail out of the seed (the writer then removes the
/// rollout, so a suspicious id fails the import instead of riding into
/// SQL). All literals here funnel through [`crate::proc::sql_quote`] /
/// [`crate::proc::sql_quote_id`]: the `sqlite3` CLI has no bind parameters.
fn qid(id: &str, sid: &str) -> Option<String> {
    match crate::proc::sql_quote_id(id) {
        Ok(q) => Some(q),
        Err(e) => {
            crate::issues::record("codex-seed", sid, &format!("suspicious id refused: {e}"));
            None
        }
    }
}

/// Seed `thread_turns` + `thread_items` + projection state for an imported
pub(super) fn backup_thread(home: &std::path::Path, sid: &str) -> anyhow::Result<()> {
    let db = codex_state_db(home)
        .ok_or_else(|| anyhow::anyhow!("Codex state database is not initialized"))?;
    crate::publication::track_rows(&db, "threads", "id", sid)?;
    let history = home.join(".codex/thread_history_1.sqlite");
    for table in [
        "thread_turns",
        "thread_items",
        "thread_history_projection_state",
    ] {
        crate::publication::track_rows(&history, table, "thread_id", sid)?;
    }
    Ok(())
}

pub(crate) fn registered(path: &std::path::Path) -> anyhow::Result<bool> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let Some(db) = codex_state_db(&home) else {
        return Ok(false);
    };
    let name = path
        .file_stem()
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    let Some(sid) = name.get(name.len().saturating_sub(36)..) else {
        return Ok(false);
    };
    let sid = crate::proc::sql_quote_id(sid)?;
    let found = crate::proc::sqlite_text(
        &db,
        &format!("SELECT 1 FROM threads WHERE id={sid};"),
        std::time::Duration::from_secs(10),
        "Codex registration check",
    )?;
    let history = home.join(".codex/thread_history_1.sqlite");
    if found != "1" || !history.is_file() {
        return Ok(false);
    }
    Ok(crate::proc::sqlite_text(
        &history,
        &format!("SELECT 1 FROM thread_history_projection_state WHERE thread_id={sid};"),
        std::time::Duration::from_secs(10),
        "Codex projection check",
    )? == "1")
}

/// Seed `thread_turns` + `thread_items` + projection state for an imported
/// rollout. Shapes mirror native rows (verified by diff): one turn per user
/// message, one projection item per rollout item, state pointing at
/// end-of-file so the recorder continues appending cleanly.
pub(super) fn seed_codex_history(
    home: &std::path::Path,
    sid: &str,
    items: &[Item],
    session: &CanonicalSession,
    byte_len: u64,
) -> bool {
    let db = home.join(".codex/thread_history_1.sqlite");
    if !db.exists() {
        return false;
    }
    let ms = session.created.timestamp_millis();
    let Some(sid_q) = qid(sid, sid) else {
        return false;
    };
    let mut stmts: Vec<String> = [
        "thread_items",
        "thread_turns",
        "thread_history_projection_state",
    ]
    .into_iter()
    .map(|table| format!("DELETE FROM {table} WHERE thread_id={sid_q};"))
    .collect();
    // group items into turns: each user message starts a turn
    let mut turns: Vec<(usize, usize)> = Vec::new();
    let mut cur: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        let is_user = item
            .thread
            .as_ref()
            .is_some_and(|(ty, _)| ty == "userMessage");
        if is_user {
            if let Some(s) = cur {
                turns.push((s, i));
            }
            cur = Some(i);
        }
    }
    if let Some(s) = cur {
        turns.push((s, items.len()));
    }
    if turns.is_empty() && !items.is_empty() {
        turns.push((0, items.len()));
    }
    let mut turn_ids: Vec<String> = Vec::new();
    for (t, (start, _end)) in turns.iter().enumerate() {
        // ordinal of the first item in this turn (meta=0, turn_context=1)
        let ord = (start + 2) as u64;
        let turn_id = format!("{sid}-t{t:04}");
        let Some(turn_q) = qid(&turn_id, sid) else {
            return false;
        };
        let first_user = items
            .get(*start)
            .map(|i| i.item_id.clone())
            .unwrap_or_default();
        let Some(first_q) = qid(&first_user, sid) else {
            return false;
        };
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_turns (thread_id, turn_id, rollout_ordinal, status, started_at, completed_at, duration_ms, first_user_item_id, final_agent_item_id, rollout_end_ordinal) VALUES ({sid_q}, {turn_q}, {ord}, 'completed', {ms}, {ms}, 0, {first_q}, NULL, {ord});"
        ));
        turn_ids.push(turn_id);
    }
    for (i, item) in items.iter().enumerate() {
        let Some((itype, thread_json)) = &item.thread else {
            continue;
        };
        let ord = (i + 2) as u64;
        // owning turn: last turn whose start <= i
        let tidx = turns.iter().rposition(|(s, _)| *s <= i).unwrap_or(0);
        let turn_id = &turn_ids[tidx];
        let Some(turn_q) = qid(turn_id, sid) else {
            return false;
        };
        let Some(iid_q) = qid(&item.item_id, sid) else {
            return false;
        };
        // Intentional fallback: a projection row must always land — an
        // unserializable item degrades to `null` JSON, and the rollout
        // itself (the source of truth) still carries the full record.
        let item_sql =
            crate::proc::sql_quote(&serde_json::to_string(thread_json).unwrap_or_default());
        let itype_q = crate::proc::sql_quote(itype);
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) VALUES ({sid_q}, {turn_q}, {iid_q}, {ord}, {ms}, {item_sql}, {itype_q});"
        ));
    }
    let next_ord = (items.len() + 2) as u64;
    stmts.push(format!(
        "INSERT OR REPLACE INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES ({sid_q}, {byte_len}, {next_ord});"
    ));
    let script = "BEGIN IMMEDIATE;\n".to_owned() + &stmts.join(";\n") + ";\nCOMMIT;\n";
    if let Err(e) = crate::proc::sqlite_exec_input(
        &db,
        script.into_bytes(),
        std::time::Duration::from_secs(30),
        "codex history seed",
    ) {
        crate::issues::record("codex-seed", sid, &format!("history seed failed: {e}"));
        return false;
    }
    true
}

/// Best-effort copy of the local codex account ids (from any native rollout).
/// Cached: the walk covers the whole sessions tree and each codex import
/// otherwise repeats it.
static CODEX_CREATOR_IDS: std::sync::LazyLock<Option<(String, String)>> =
    std::sync::LazyLock::new(|| dirs::home_dir().as_deref().and_then(find_creator_ids));

pub(super) fn local_creator_ids() -> Option<(String, String)> {
    CODEX_CREATOR_IDS.clone()
}

fn find_creator_ids(home: &std::path::Path) -> Option<(String, String)> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in walkdir::WalkDir::new(home.join(".codex/sessions"))
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = e.path();
        if !e.file_type().is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // native rollouts: rollout-<ts>-<uuid>.jsonl (hyphen before uuid)
        if !name.starts_with("rollout-") || name.contains('_') {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let Ok(mtime) = md.modified() else { continue };
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, p.to_path_buf()));
        }
    }
    let (_, path) = newest?;
    let line = std::io::BufRead::lines(std::io::BufReader::new(fs::File::open(path).ok()?))
        .next()?
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    let uid = v.pointer("/payload/creator_user_id")?.as_str()?.to_string();
    let aid = v
        .pointer("/payload/creator_account_id")?
        .as_str()?
        .to_string();
    Some((uid, aid))
}

/// Insert a `threads` row for an imported rollout so codex's picker lists it.
pub(super) fn register_codex_thread(
    home: &std::path::Path,
    sid: &str,
    rollout: &std::path::Path,
    session: &CanonicalSession,
    title: &str,
) -> bool {
    let Some(db) = codex_state_db(home) else {
        return false;
    };
    let ts = session.created.timestamp();
    let ts_ms = session.created.timestamp_millis();
    // Free text (titles, paths) quotes by doubling; the id is allowlisted.
    let title_sql = crate::proc::sql_quote(title);
    let cwd_sql = crate::proc::sql_quote(&session.cwd.to_string_lossy());
    let path_sql = crate::proc::sql_quote(&rollout.to_string_lossy());
    let sid_sql = match crate::proc::sql_quote_id(sid) {
        Ok(q) => q,
        Err(e) => {
            crate::issues::record("codex-register", sid, &e.to_string());
            return false;
        }
    };
    // codex's picker filters on its known `source`/`originator` enum values —
    // an unknown value makes the row invisible (verified empirically).
    // `name` is what the picker row renders; without it the row falls back to
    // the unprefixed first message even though title/preview carry [agent].
    let sql = format!(
        "INSERT OR REPLACE INTO threads \
         (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
          sandbox_policy, approval_mode, has_user_event, archived, cli_version, \
          first_user_message, preview, created_at_ms, updated_at_ms, recency_at, recency_at_ms, \
          thread_source, originator, history_mode, memory_mode, name) VALUES \
         ({sid_sql}, {path_sql}, {ts}, {ts}, 'cli', 'openai', {cwd_sql}, {title_sql}, \
          '{{\"type\":\"disabled\"}}', 'never', 1, 0, '0.0.0-xses', \
          {title_sql}, {title_sql}, {ts_ms}, {ts_ms}, {ts}, {ts_ms}, \
          'user', 'codex-tui', 'paginated', 'enabled', {title_sql});"
    );
    crate::proc::sqlite_exec(
        &db,
        &sql,
        std::time::Duration::from_secs(30),
        "codex thread registration",
    )
    .is_ok()
}

/// Remove an obsolete generation's picker and projection entries. The index
/// retains failed retirements, so a locked database is retried on the next sync.
pub(crate) fn retire_codex(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let home = dirs::home_dir().context("no home dir")?;
    let name = path
        .file_stem()
        .and_then(|p| p.to_str())
        .context("invalid rollout path")?;
    let sid_raw = name
        .get(name.len().saturating_sub(36)..)
        .context("invalid rollout id")?;
    let sid = crate::proc::sql_quote_id(sid_raw)?;
    let mut queries = Vec::new();
    if let Some(db) = codex_state_db(&home) {
        queries.push((db, format!("DELETE FROM threads WHERE id={sid};")));
    }
    let history = home.join(".codex/thread_history_1.sqlite");
    if history.exists() {
        queries.push((history, format!("BEGIN IMMEDIATE; DELETE FROM thread_items WHERE thread_id={sid}; DELETE FROM thread_turns WHERE thread_id={sid}; DELETE FROM thread_history_projection_state WHERE thread_id={sid}; COMMIT;")));
    }
    for (db, sql) in queries {
        crate::proc::sqlite_exec(
            &db,
            &sql,
            std::time::Duration::from_secs(10),
            "Codex retirement",
        )?;
    }
    Ok(())
}

/// Find the codex state DB that carries the `threads` table.
fn codex_state_db(home: &std::path::Path) -> Option<PathBuf> {
    let dir = home.join(".codex");
    let mut candidates: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("state") && n.ends_with(".sqlite"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.reverse();
    for db in candidates {
        let probe = crate::proc::sqlite_text(
            &db,
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='threads';",
            std::time::Duration::from_secs(10),
            "codex state DB probe",
        );
        if probe.is_ok_and(|out| out == "1") {
            return Some(db);
        }
    }
    None
}
