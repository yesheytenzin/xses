//! codex writer: rollout file + `threads` row + history projections.
use super::{atomic_write, first_ask, iso, iso_stamp, push, role_str, synth_id};
use crate::canonical::CanonicalSession;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
/// Best-effort copy of the local codex account ids (from any native rollout).
/// Cached: the walk covers the whole sessions tree and each codex import
/// otherwise repeats it.
static CODEX_CREATOR_IDS: std::sync::LazyLock<Option<(String, String)>> =
    std::sync::LazyLock::new(|| dirs::home_dir().as_deref().and_then(find_creator_ids));

pub(super) fn write(
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sid = synth_id(session.tool, &session.id);
    // codex vets the filename: `rollout-<ts>-<full-uuid>.jsonl` ("canonical
    // rollout filename"), so the uuid must be in it, not a short id.
    let target = home
        .join(".codex/sessions")
        .join(session.created.format("%Y/%m/%d").to_string())
        .join(format!(
            "rollout-{}-{}.jsonl",
            iso_stamp(session.created),
            sid
        ));
    let ts = iso(&session.created);
    let mut buf = Vec::new();
    // codex's thread/read rejects a rollout whose first line isn't a fully
    // populated session_meta ("does not start with session metadata") —
    // verified by replaying resume against drop-in files.
    let mut meta = json!({
        "session_id": sid,
        "id": sid,
        "timestamp": ts,
        "cwd": session.cwd,
        "runtime_workspace_roots": [session.cwd],
        "originator": "codex-tui",
        "cli_version": "0.0.0-xses",
        "source": "cli",
        "thread_source": "user",
        "model_provider": "openai",
        "history_mode": "paginated",
        "base_instructions": {"text": "Imported by xses."},
        "xses": {"source": session.tool.as_str(), "srcId": session.id},
        // NOTE: `git`/`context_window` are omitted on purpose — an empty
        // `context_window: {}` fails thread/read deserialization (verified).
    });
    // impersonate the local account so thread/read's ownership checks pass
    if let Some((uid, aid)) = local_creator_ids() {
        meta["creator_user_id"] = json!(uid);
        meta["creator_account_id"] = json!(aid);
    }
    push(
        &mut buf,
        &json!({ "ordinal": 0, "type": "session_meta", "payload": meta, "timestamp": ts }),
    );
    // turn_context carries the workspace the TUI validates on resume
    push(
        &mut buf,
        &json!({
            "ordinal": 1,
            "type": "turn_context",
            "timestamp": ts,
            "payload": {
                "cwd": session.cwd,
                "approval_policy": "never",
                "sandbox_policy": {"type": "disabled"},
                "model": "imported",
            }
        }),
    );
    for (i, (role, text)) in msgs.iter().enumerate() {
        push(
            &mut buf,
            &json!({
                "ordinal": i + 2,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": format!("msg_{i}"),
                    "role": role_str(*role),
                    "content": [{"type": if matches!(role, crate::canonical::Role::Assistant) {"output_text"} else {"input_text"}, "text": text}],
                },
                "timestamp": ts,
            }),
        );
    }
    atomic_write(&target, &buf).ok()?;
    // codex's own picker lists from the `threads` table, not the sessions dir:
    // register the drop-in file so `codex resume` sees it.
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(msgs));
    if !register_codex_thread(&home, &sid, &target, session, &title) {
        remove_codex_metadata(&home, &sid);
        let _ = fs::remove_file(&target);
        return None;
    }
    // the TUI transcript renders from `thread_history_1.sqlite` projections,
    // never built for drop-in rollouts — seed them so resume shows content.
    let item_ids: Vec<String> = (0..msgs.len()).map(|i| format!("msg_{i}")).collect();
    if !seed_codex_history(&home, &sid, &item_ids, session, msgs, buf.len() as u64) {
        remove_codex_metadata(&home, &sid);
        let _ = fs::remove_file(&target);
        return None;
    }
    Some(target)
}
/// Seed `thread_turns` + `thread_items` + projection state for an imported
/// rollout. Shapes mirror native rows (verified by diff): one turn per user
/// message, one userMessage/agentMessage item per message, state pointing at
/// end-of-file so the recorder continues appending cleanly.
fn seed_codex_history(
    home: &std::path::Path,
    sid: &str,
    item_ids: &[String],
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
    byte_len: u64,
) -> bool {
    let db = home.join(".codex/thread_history_1.sqlite");
    if !db.exists() {
        return false;
    }
    let ms = session.created.timestamp_millis();
    let esc = |s: &str| s.replace('\'', "''");
    let mut stmts: Vec<String> = Vec::new();
    // group messages into turns: each user message starts a turn
    let mut turns: Vec<(usize, usize)> = Vec::new();
    let mut cur: Option<usize> = None;
    for (i, (role, _)) in msgs.iter().enumerate() {
        if matches!(role, crate::canonical::Role::User) {
            if let Some(s) = cur {
                turns.push((s, i));
            }
            cur = Some(i);
        }
    }
    if let Some(s) = cur {
        turns.push((s, msgs.len()));
    }
    if turns.is_empty() && !msgs.is_empty() {
        turns.push((0, msgs.len()));
    }
    let mut turn_ids: Vec<String> = Vec::new();
    for (t, (start, _end)) in turns.iter().enumerate() {
        // ordinal of the first message in this turn (meta=0, turn_context=1)
        let ord = (start + 2) as u64;
        let turn_id = format!("{sid}-t{t:04}");
        let first_user = item_ids.get(*start).cloned().unwrap_or_default();
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_turns (thread_id, turn_id, rollout_ordinal, status, started_at, completed_at, duration_ms, first_user_item_id, final_agent_item_id, rollout_end_ordinal) VALUES ('{sid}', '{turn_id}', {ord}, 'completed', {ms}, {ms}, 0, '{first_user}', NULL, {ord});"
        ));
        turn_ids.push(turn_id);
    }
    for (i, (role, text)) in msgs.iter().enumerate() {
        let ord = (i + 2) as u64;
        // owning turn: last turn whose start <= i
        let tidx = turns.iter().rposition(|(s, _)| *s <= i).unwrap_or(0);
        let turn_id = &turn_ids[tidx];
        let iid = &item_ids[i];
        let (itype, item) = if matches!(role, crate::canonical::Role::Assistant) {
            (
                "agentMessage",
                json!({"type": "agentMessage", "id": iid, "text": text}),
            )
        } else {
            (
                "userMessage",
                json!({"type": "userMessage", "id": iid, "content": [{"type": "text", "text": text}]}),
            )
        };
        let item_sql = esc(&serde_json::to_string(&item).unwrap_or_default());
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) VALUES ('{sid}', '{turn_id}', '{iid}', {ord}, {ms}, '{item_sql}', '{itype}');"
        ));
    }
    let next_ord = (msgs.len() + 2) as u64;
    stmts.push(format!(
        "INSERT OR REPLACE INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES ('{sid}', {byte_len}, {next_ord});"
    ));
    let script = stmts.join(";\n") + ";\n";
    // pipe via stdin: large sessions exceed argv limits as a single arg
    let mut child = match std::process::Command::new("sqlite3")
        .args(["-cmd", ".timeout 5000"])
        .arg(&db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            crate::issues::record("codex-seed", sid, &format!("spawn failed: {e}"));
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if stdin.write_all(script.as_bytes()).is_err() {
            crate::issues::record("codex-seed", sid, "script write failed");
            return false;
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => return true,
        Ok(out) => crate::issues::record(
            "codex-seed",
            sid,
            &format!(
                "history seed failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ),
        Err(e) => crate::issues::record("codex-seed", sid, &format!("wait failed: {e}")),
    }
    false
}
fn local_creator_ids() -> Option<(String, String)> {
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
fn register_codex_thread(
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
    let title_sql = title.replace('\'', "''");
    let cwd_sql = session.cwd.to_string_lossy().replace('\'', "''");
    let path_sql = rollout.to_string_lossy().replace('\'', "''");
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
         ('{sid}', '{path_sql}', {ts}, {ts}, 'cli', 'openai', '{cwd_sql}', '{title_sql}', \
          '{{\"type\":\"disabled\"}}', 'never', 1, 0, '0.0.0-xses', \
          '{title_sql}', '{title_sql}', {ts_ms}, {ts_ms}, {ts}, {ts_ms}, \
          'user', 'codex-tui', 'paginated', 'enabled', '{title_sql}');"
    );
    let Ok(out) = std::process::Command::new("sqlite3")
        .args(["-cmd", ".timeout 5000"])
        .arg(&db)
        .arg(&sql)
        .output()
    else {
        return false;
    };
    out.status.success()
}
fn remove_codex_metadata(home: &std::path::Path, sid: &str) {
    if let Some(db) = codex_state_db(home) {
        let sid_sql = sid.replace('\'', "''");
        let sql = format!("DELETE FROM threads WHERE id = '{sid_sql}';");
        let _ = std::process::Command::new("sqlite3")
            .args(["-cmd", ".timeout 5000"])
            .arg(&db)
            .arg(&sql)
            .output();
    }
    let hist = home.join(".codex/thread_history_1.sqlite");
    if hist.exists() {
        let sid_sql = sid.replace('\'', "''");
        let sql = format!(
            "DELETE FROM thread_items WHERE thread_id = '{sid_sql}'; \
             DELETE FROM thread_turns WHERE thread_id = '{sid_sql}'; \
             DELETE FROM thread_history_projection_state WHERE thread_id = '{sid_sql}';"
        );
        let _ = std::process::Command::new("sqlite3")
            .args(["-cmd", ".timeout 5000"])
            .arg(&hist)
            .arg(&sql)
            .output();
    }
}
/// Find the codex state DB that carries the `threads` table.
pub(crate) fn codex_state_db_pub(home: &std::path::Path) -> Option<PathBuf> {
    codex_state_db(home)
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
        let probe = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='threads';")
            .output()
            .ok()?;
        if probe.status.success() && String::from_utf8_lossy(&probe.stdout).trim() == "1" {
            return Some(db);
        }
    }
    None
}
