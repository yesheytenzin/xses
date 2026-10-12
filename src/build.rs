//! Build: IR (`CanonicalSession`) → target tool's native session records.
//!
//! Writers are additive-only and atomic: temp file → fsync → rename, with a
//! `.bak` of any pre-existing target first. Deterministic synthetic ids keep
//! re-imports idempotent.
use crate::canonical::{Block, CanonicalSession, LossReport, Role};
use crate::model::Tool;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn command_output_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command exceeded {} seconds", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Atomic writer: writes bytes to `path` via temp + rename, backing up any
/// existing file. Returns the fidelity report of what was written.
pub fn build_session(target: Tool, session: &CanonicalSession) -> (Vec<PathBuf>, LossReport) {
    let (messages, mut report) = session.messages();

    // Writers that cannot represent loss are declared up front (e.g. claude has
    // no storage for raw tool-result output beyond text).
    let files = match target {
        Tool::Pi | Tool::Omp => vec![write_pi(target, session, &messages)],
        Tool::Claude => vec![write_claude(session, &messages)],
        Tool::Codex => vec![write_codex(session, &messages)],
        Tool::Opencode => vec![write_opencode(session, &messages)],
    };
    for f in &files {
        if f.is_none() {
            report.dropped += session.message_count();
        }
    }
    let files: Vec<PathBuf> = files.into_iter().flatten().collect();

    (files, report)
}

/// Apply `--scope last:N`: keep only the trailing N user+text turns.
pub fn scope_last_n(session: CanonicalSession, n: usize) -> CanonicalSession {
    let users: Vec<usize> = session
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            Block::Text {
                role: Role::User, ..
            } => Some(i),
            _ => None,
        })
        .collect();
    if n == 0 {
        return CanonicalSession {
            blocks: vec![],
            ..session
        };
    }
    if users.is_empty() {
        return CanonicalSession {
            blocks: vec![],
            ..session
        };
    }
    if n >= users.len() {
        return session;
    }
    let start = users[users.len() - n];
    let blocks: Vec<Block> = session
        .blocks
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= start)
        .map(|(_, b)| b.clone())
        .collect();
    CanonicalSession { blocks, ..session }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("no parent")?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        let bak = path.with_extension("bak");
        if !bak.exists() {
            fs::copy(path, &bak)?;
        }
    }
    let tmp = parent.join(format!(
        ".xses-{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Ok(dir) = File::open(parent) {
        dir.sync_all().ok();
    }
    Ok(())
}

use std::fs::File;

fn write_pi(
    target: Tool,
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let is_omp = matches!(target, Tool::Omp);
    // pi wraps the absolute path slug; omp strips home then wraps segments.
    // The store is chosen by the TARGET, not the source.
    let (base, slug) = match target {
        Tool::Pi => (
            home.join(".pi/agent/sessions"),
            // native: `-` + `<cwd>/`.replace('/','-') + `-`  →  `--a-b-c--`
            format!(
                "-{}-",
                format!("{}/", session.cwd.to_string_lossy()).replace('/', "-")
            ),
        ),
        _ => (home.join(".omp/agent/sessions"), {
            let rel = session
                .cwd
                .strip_prefix(&home)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| session.cwd.to_string_lossy().to_string());
            format!("-{}", rel.trim_start_matches('/').replace('/', "-"))
        }),
    };
    let target = base.join(slug).join(format!(
        "{}_{}.jsonl",
        iso_stamp(session.created),
        if is_omp {
            omp_id(session)
        } else {
            import_id(session)
        }
    ));
    let sid = if is_omp {
        omp_id(session)
    } else {
        synth_id(session)
    };
    let ts = iso(&session.created);
    let ts_ms = session.created.timestamp_millis();
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(msgs));
    let mut buf = Vec::new();
    let mut title_event = json!({
        "type": "title",
        "v": 1,
        "title": title,
        "source": if is_omp { "auto" } else { "xses" },
    });
    if is_omp {
        // OMP validates these fields on the first title record; Pi accepts the
        // smaller title shape.
        title_event["updatedAt"] = json!(ts);
        title_event["pad"] = json!(" ".repeat(127));
    }
    if is_omp {
        push(&mut buf, &title_event);
    }
    let mut session_header = json!({
        "type": "session",
        "version": 3,
        "id": sid,
        "timestamp": ts,
        "cwd": session.cwd,
    });
    if !is_omp {
        session_header["title"] = json!(title);
        session_header["titleSource"] = json!("xses");
        session_header["xses"] = json!({
            "source": session.tool.as_str(),
            "srcId": session.id,
        });
    }
    push(&mut buf, &session_header);
    if !is_omp {
        // Pi's resume scanner requires the session header to be the first
        // record; it still discovers this title record immediately after it.
        push(&mut buf, &title_event);
    }
    // Native sessions open with a model + thinking-level chain; pi's TUI
    // renders session stats off these and off per-message `usage` (both
    // verified against a live session — missing usage crashes the renderer).
    let model_id = format!("{sid}-mdl");
    let think_id = format!("{sid}-thk");
    let model_change = if is_omp {
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": ts,
            "model": "xses/imported",
            "role": "default",
            "resolvedModelIsFallback": false,
        })
    } else {
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": ts,
            "provider": "xses",
            "modelId": "imported",
        })
    };
    push(&mut buf, &model_change);
    let thinking_change = if is_omp {
        json!({
            "type": "thinking_level_change",
            "id": think_id,
            "parentId": model_id,
            "timestamp": ts,
            "thinkingLevel": "off",
            "configured": null,
        })
    } else {
        json!({
            "type": "thinking_level_change",
            "id": think_id,
            "parentId": model_id,
            "timestamp": ts,
            "thinkingLevel": "off",
        })
    };
    push(&mut buf, &thinking_change);
    let mut parent: Option<String> = Some(think_id);
    for (i, (role, text)) in msgs.iter().enumerate() {
        let mid = format!("{sid}-m{i:04}");
        let mut message = json!({
            "role": role_str(*role),
            "content": [{"type": "text", "text": text}],
            "timestamp": ts_ms,
        });
        if matches!(role, crate::canonical::Role::Assistant) {
            message["api"] = json!("xses");
            message["provider"] = json!("xses");
            message["model"] = json!("imported");
            message["stopReason"] = json!("stop");
            message["thinkingLevel"] = json!("off");
            message["durationMs"] = json!(0);
            message["usage"] = json!({
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
            });
        }
        push(
            &mut buf,
            &json!({
                "type": "message",
                "id": mid,
                "parentId": parent,
                "timestamp": ts,
                "message": message,
            }),
        );
        parent = Some(mid);
    }
    atomic_write(&target, &buf).ok()?;
    Some(target)
}

fn write_claude(
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let slug = session.cwd.to_string_lossy().replace('/', "-");
    let sid = synth_id(session);
    // Claude's /resume only lists files whose name == the session UUID, and it
    // displays the `last-prompt` text (verified empirically).
    let target = home
        .join(".claude/projects")
        .join(slug)
        .join(format!("{sid}.jsonl"));
    let mut buf = Vec::new();

    // Real uuids: the picker's leafUuid/parent chains must be UUID-shaped.
    let mut uuids: Vec<String> = Vec::with_capacity(msgs.len());
    for i in 0..msgs.len() {
        uuids.push(uuid_like(&sid, i));
    }
    push(
        &mut buf,
        &json!({
            "type": "last-prompt",
            "lastPrompt": format!("[{}] {}", session.tool.as_str(), first_ask(msgs)),
            "leafUuid": uuids.last().cloned().unwrap_or_else(|| sid.clone()),
            "sessionId": sid,
            "xses": {"source": session.tool.as_str(), "srcId": session.id},
        }),
    );
    let mut parent: Option<String> = None;
    for (i, (role, text)) in msgs.iter().enumerate() {
        let uuid = uuids[i].clone();
        push(
            &mut buf,
            &json!({
                "type": role_str(*role),
                "message": {"role": role_str(*role), "content": text},
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": iso(&session.created),
                "cwd": session.cwd,
                "sessionId": sid,
                "isSidechain": false,
                "userType": "external",
            }),
        );
        parent = Some(uuid);
    }
    atomic_write(&target, &buf).ok()?;
    Some(target)
}

/// Deterministic UUID-shaped id derived from the session id + index.
fn uuid_like(seed: &str, i: usize) -> String {
    let mut h = [0x811c9dc5u32; 4];
    let key = format!("{seed}:{i}");
    for (idx, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[idx % 4] ^= b;
        h[idx % 4] = h[idx % 4].wrapping_mul(0x01000193);
    }
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        h[0],
        h[1] & 0xffff,
        h[2] & 0xfff,
        h[3] & 0xfff,
        (((h[1] as u64) << 32) | h[0] as u64) & 0xffff_ffff_ffff
    )
}

fn omp_id(session: &CanonicalSession) -> String {
    // OMP validates session IDs as UUIDv7: the first 48 bits are the
    // millisecond timestamp, followed by the version and RFC 9562 variant.
    // Keep the remaining bits deterministic so repeated syncs retain identity.
    let base: String = synth_id(session).chars().filter(|c| *c != '-').collect();
    let timestamp = (session.created.timestamp_millis() as u64) & 0x0000_ffff_ffff_ffff;
    let mut hex = format!("{timestamp:012x}{}", &base[12..]);
    let mut chars: Vec<char> = hex.drain(..).collect();
    chars[12] = '7';
    chars[16] = '8';
    format!(
        "{}-{}-{}-{}-{}",
        chars[0..8].iter().collect::<String>(),
        chars[8..12].iter().collect::<String>(),
        chars[12..16].iter().collect::<String>(),
        chars[16..20].iter().collect::<String>(),
        chars[20..32].iter().collect::<String>()
    )
}

fn write_codex(
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sid = synth_id(session);
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
    if let Some((uid, aid)) = local_creator_ids(&home) {
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
            eprintln!("codex history seed spawn failed: {e}");
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if stdin.write_all(script.as_bytes()).is_err() {
            eprintln!("codex history seed write failed for {sid}");
            return false;
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => return true,
        Ok(out) => eprintln!(
            "codex history seed failed for {sid}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => eprintln!("codex history seed wait failed: {e}"),
    }
    false
}

/// Best-effort copy of the local codex account ids (from any native rollout).
fn local_creator_ids(home: &std::path::Path) -> Option<(String, String)> {
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
    let sql = format!(
        "INSERT OR REPLACE INTO threads \
         (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, \
          sandbox_policy, approval_mode, has_user_event, archived, cli_version, \
          first_user_message, preview, created_at_ms, updated_at_ms, recency_at, recency_at_ms, \
          thread_source, originator, history_mode, memory_mode) VALUES \
         ('{sid}', '{path_sql}', {ts}, {ts}, 'cli', 'openai', '{cwd_sql}', '{title_sql}', \
          '{{\"type\":\"disabled\"}}', 'never', 1, 0, '0.0.0-xses', \
          '{title_sql}', '{title_sql}', {ts_ms}, {ts_ms}, {ts}, {ts_ms}, \
          'user', 'codex-tui', 'paginated', 'enabled');"
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
pub fn codex_state_db_pub(home: &std::path::Path) -> Option<PathBuf> {
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

/// Export the IR into portable interchange formats (unisessions-style traces).
pub fn export_trace(session: &CanonicalSession, format: &str) -> Result<Vec<u8>> {
    let (messages, _report) = session.messages();
    match format {
        "sharegpt" => {
            let rows: Vec<Value> = messages
                .iter()
                .map(|(role, text)| json!({"from": role_str(*role), "value": text}))
                .collect();
            Ok(serde_json::to_vec_pretty(&json!({"conversations": rows}))?)
        }
        "openai" => {
            let rows: Vec<Value> = messages
                .iter()
                .map(|(role, text)| json!({"role": role_str(*role), "content": text}))
                .collect();
            Ok(serde_json::to_vec_pretty(&json!({"messages": rows}))?)
        }
        other => anyhow::bail!("unknown trace format {other:?} (want sharegpt|openai)"),
    }
}

fn push(buf: &mut Vec<u8>, v: &Value) {
    buf.extend(serde_json::to_vec(v).unwrap_or_default());
    buf.push(b'\n');
}

fn role_str(r: crate::canonical::Role) -> &'static str {
    match r {
        crate::canonical::Role::User => "user",
        crate::canonical::Role::Assistant => "assistant",
        crate::canonical::Role::System => "system",
    }
}

fn first_ask(msgs: &[(crate::canonical::Role, String)]) -> String {
    msgs.iter()
        .find(|(r, _)| matches!(r, crate::canonical::Role::User))
        .map(|(_, t)| {
            t.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(120)
                .collect::<String>()
        })
        .unwrap_or_else(|| "imported".into())
}

/// Deterministic synthetic session id (idempotent re-imports).
fn synth_id(session: &CanonicalSession) -> String {
    let mut h = [0x811c9dc5u32; 4];
    let key = format!("{}:{}", session.tool.as_str(), session.id);
    for (i, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[i % 4] ^= b;
        h[i % 4] = h[i % 4].wrapping_mul(0x01000193);
    }
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        h[0],
        h[1] & 0xffff,
        h[2] & 0xfff,
        h[3] & 0xfff,
        (((h[1] as u64) << 32) | h[0] as u64) & 0xffff_ffff_ffff
    )
}

fn import_id(session: &CanonicalSession) -> String {
    synth_id(session)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect()
}

fn iso(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn iso_stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// opencode: synthesize an export-format payload and pipe it through
/// `opencode import`. We never touch opencode's SQLite store directly — the
/// CLI owns all schema knowledge (showagent's proven policy).
/// The model the user actually runs in opencode (from the most recent
/// assistant message in its DB), resolved lazily; falls back to the
/// default when the store is empty.
static OPENCODE_MODEL: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    let fallback = json!({"providerID": "opencode", "modelID": "big-pickle", "id": "big-pickle"});
    let query =
        "SELECT json_extract(data,'$.providerID') AS p, json_extract(data,'$.modelID') AS m \
                 FROM message WHERE json_extract(data,'$.role')='assistant' \
                 AND json_extract(data,'$.modelID') IS NOT NULL \
                 ORDER BY time_created DESC LIMIT 1;";
    let _cli = crate::model::OPENCODE_CLI_LOCK.lock();
    let Ok(out) = std::process::Command::new("opencode")
        .args(["db", "--format", "json", query])
        .output()
    else {
        return fallback;
    };
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return fallback;
    };
    match rows.first() {
        Some(r) => {
            let p = r.get("p").and_then(|v| v.as_str()).unwrap_or("opencode");
            let m = r.get("m").and_then(|v| v.as_str()).unwrap_or("big-pickle");
            // the import validator requires `model.id` as well
            json!({"providerID": p, "modelID": m, "id": m})
        }
        None => fallback,
    }
});

fn local_opencode_model() -> serde_json::Value {
    OPENCODE_MODEL.clone()
}

fn write_opencode(session: &CanonicalSession, msgs: &[(Role, String)]) -> Option<PathBuf> {
    let ses = format!("ses_{}", import_id(session));
    let now_ms = chrono::Utc::now().timestamp_millis();
    let model = local_opencode_model();
    let provider_id = model
        .get("providerID")
        .and_then(|v| v.as_str())
        .unwrap_or("opencode")
        .to_string();
    let model_id = model
        .get("modelID")
        .and_then(|v| v.as_str())
        .unwrap_or("big-pickle")
        .to_string();
    // Native shapes (verified against a live export): assistant infos carry
    // parentID/mode/path/cost/tokens + top-level providerID/modelID, user infos
    // carry agent/model/summary and no parentID.
    let mut prev_id: Option<String> = None;
    let mut msgs_json: Vec<serde_json::Value> = Vec::with_capacity(msgs.len());
    for (i, (role, text)) in msgs.iter().enumerate() {
        // opencode orders messages by (time_created, id): ids must sort in
        // transcript order and timestamps must increase, or the import shows
        // up scrambled.
        let mid = format!("msg_{i:06}-{}", mint_suffix());
        let ts = now_ms - ((msgs.len() - i) as i64);
        let info = if matches!(role, Role::Assistant) {
            json!({
                "id": mid,
                "sessionID": ses,
                "role": "assistant",
                "parentID": prev_id.clone().unwrap_or_else(|| format!("msg_{:06}-seed", 0)),
                "time": {"created": ts},
                "mode": "build",
                "agent": "build",
                "path": {"cwd": session.cwd, "root": session.cwd},
                "cost": 0,
                "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "providerID": provider_id,
                "modelID": model_id,
            })
        } else {
            let mut info = json!({
                "id": mid,
                "sessionID": ses,
                "role": role_str(*role),
                "time": {"created": ts},
                "agent": "build",
                "model": model,
                "summary": {"diffs": []},
            });
            if let Some(prev) = &prev_id {
                info["parentID"] = json!(prev);
            }
            info
        };
        msgs_json.push(json!({
            "info": info,
            "parts": [{
                "type": "text",
                "id": mint_id("prt_"),
                "sessionID": ses,
                "messageID": mid,
                "text": text,
            }],
        }));
        prev_id = Some(mid);
    }
    let payload = json!({
        "info": {
            "id": ses,
        "slug": format!("xses-{}", import_id(session)),
            "projectID": "global",
            "directory": session.cwd,
            "path": session.cwd.to_string_lossy().trim_start_matches('/'),
            "title": format!("[{}] {}", session.tool.as_str(), first_ask(msgs)),
            "version": "1.0.0",
            "time": {"created": now_ms, "updated": now_ms},
            "agent": "build",
            "model": model,
            "summary": {"diffs": [], "additions": 0, "deletions": 0, "files": 0},
            "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "xses": {"source": session.tool.as_str(), "srcId": session.id},
        },
        "messages": msgs_json,
    });
    let tmp = std::env::temp_dir().join(format!("xses-{ses}.json"));
    let bytes = serde_json::to_vec_pretty(&payload).ok()?;
    fs::write(&tmp, &bytes).ok()?;
    let _cli = crate::model::OPENCODE_CLI_LOCK.lock();
    let mut command = Command::new("opencode");
    command
        .arg("import")
        .arg(&tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = command_output_timeout(command, Duration::from_secs(12));
    let imported = out
        .ok()
        .map(|out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|line| line.trim_start().starts_with("Imported session:"))
        })
        .unwrap_or(false);
    if !imported {
        fs::remove_file(&tmp).ok();
        return None;
    }
    fs::remove_file(&tmp).ok();
    Some(PathBuf::from(format!("opencode://{ses}")))
}

fn mint_id(prefix: &str) -> String {
    format!("{prefix}{}", mint_suffix())
}

/// 24 random alphanumerics.
fn mint_suffix() -> String {
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();
    let mut s = String::new();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as usize;
    for _ in 0..24 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[(seed >> 33) % alphabet.len()]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keeps_non_text_blocks_in_retained_turn() {
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Claude,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            blocks: vec![
                Block::Text {
                    role: Role::User,
                    text: "one".into(),
                },
                Block::ToolCall {
                    name: "shell".into(),
                    args: "{}".into(),
                },
                Block::Text {
                    role: Role::Assistant,
                    text: "two".into(),
                },
                Block::Text {
                    role: Role::User,
                    text: "three".into(),
                },
                Block::Thinking {
                    text: "thinking".into(),
                },
                Block::Text {
                    role: Role::Assistant,
                    text: "four".into(),
                },
            ],
        };
        let scoped = scope_last_n(session, 1);
        assert_eq!(scoped.blocks.len(), 3);
        assert!(matches!(
            scoped.blocks[0],
            Block::Text {
                role: Role::User,
                ..
            }
        ));
        assert!(matches!(scoped.blocks[1], Block::Thinking { .. }));
    }

    #[test]
    fn omp_id_is_timestamped_uuidv7() {
        let created = DateTime::<Utc>::from_timestamp(1_700_000_000, 123_000_000).unwrap();
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Pi,
            cwd: "/tmp/project".into(),
            created,
            blocks: vec![],
        };
        let id = omp_id(&session);
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'7');
        assert_eq!(id.as_bytes()[19], b'8');
        assert_eq!(&id[..13], "018bcfe5-687b");
    }
}
