//! codex writer: rollout file + `threads` row + history projections.
//!
//! The rollout carries codex's own item vocabulary — `message` with
//! `input_text`/`output_text`/`input_image`, `reasoning`, `function_call`,
//! `function_call_output`, `custom_tool_call(_output)`, and `token_count`
//! events — while the TUI projection (`thread_items`) uses the item types the
//! picker renders: `userMessage`, `agentMessage`, `commandExecution`,
//! `contextCompaction`.
use super::{atomic_write, first_ask, iso, iso_stamp, manifest, push, synth_id};
use crate::canonical::{Block, CanonicalSession, Role};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

/// One rollout record plus (optionally) the projection row that mirrors it.
struct Item {
    /// Wrapper record type in the rollout: `response_item` or `event_msg`.
    wrapper: &'static str,
    payload: Value,
    /// `(thread_items.item_type, item_json)`; `None` keeps the item out of the
    /// projection (ordinals may have gaps — the projection index allows it).
    thread: Option<(String, Value)>,
    item_id: String,
}

pub(super) fn write(session: &CanonicalSession) -> Option<PathBuf> {
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
    let items = rollout_items(session);
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
        "xses": manifest(session),
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
    for (i, item) in items.iter().enumerate() {
        push(
            &mut buf,
            &json!({
                "ordinal": i + 2,
                "type": item.wrapper,
                "payload": item.payload,
                "timestamp": ts,
            }),
        );
    }
    atomic_write(&target, &buf).ok()?;
    // codex's own picker lists from the `threads` table, not the sessions dir:
    // register the drop-in file so `codex resume` sees it.
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(session));
    if !register_codex_thread(&home, &sid, &target, session, &title) {
        remove_codex_metadata(&home, &sid);
        let _ = fs::remove_file(&target);
        return None;
    }
    // the TUI transcript renders from `thread_history_1.sqlite` projections,
    // never built for drop-in rollouts — seed them so resume shows content.
    if !seed_codex_history(&home, &sid, &items, session, buf.len() as u64) {
        remove_codex_metadata(&home, &sid);
        let _ = fs::remove_file(&target);
        return None;
    }
    Some(target)
}

/// Map the IR onto codex's item vocabulary, keeping one entry per rollout
/// record so ordinals stay aligned with the projection.
fn rollout_items(session: &CanonicalSession) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut n = 0usize;
    let mint = |prefix: &str, n: &mut usize| {
        *n += 1;
        format!("{prefix}{n:05}")
    };

    // Import note: the resumed agent must read the transcript as history.
    {
        let id = mint("msg_", &mut n);
        out.push(Item {
            wrapper: "response_item",
            payload: json!({
                "type": "message",
                "id": id,
                "role": "developer",
                "content": [{"type": "input_text", "text": super::pi::IMPORT_NOTE}],
            }),
            thread: Some((
                "agentMessage".into(),
                json!({"type": "agentMessage", "id": id, "text": super::pi::IMPORT_NOTE}),
            )),
            item_id: id,
        });
    }

    // Pending shell calls, so a result can complete its item.
    let mut pending: Vec<(Option<String>, String, String)> = Vec::new();
    for (_, blocks) in session.grouped() {
        for block in &blocks {
            match block {
                Block::Text { role, text, .. } => {
                    let id = mint("msg_", &mut n);
                    let (wrapper_role, text_type) = match role {
                        Role::User => ("user", "input_text"),
                        Role::System => ("system", "input_text"),
                        Role::Assistant => ("assistant", "output_text"),
                    };
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "message",
                            "id": id,
                            "role": wrapper_role,
                            "content": [{"type": text_type, "text": text}],
                        }),
                        thread: Some(if matches!(role, Role::User) {
                            (
                                "userMessage".into(),
                                json!({"type": "userMessage", "id": id,
                                       "content": [{"type": "text", "text": text}]}),
                            )
                        } else {
                            (
                                "agentMessage".into(),
                                json!({"type": "agentMessage", "id": id, "text": text}),
                            )
                        }),
                        item_id: id,
                    });
                }
                Block::Thinking { text, .. } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let id = mint("rs_", &mut n);
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "reasoning",
                            "id": id,
                            "summary": [{"type": "summary_text", "text": text}],
                            "content": [],
                        }),
                        // codex's own reasoning items project as empty, so the
                        // projection shows the text as an agent message.
                        thread: Some((
                            "agentMessage".into(),
                            json!({"type": "agentMessage", "id": id, "text": format!("[thinking]\n{text}")}),
                        )),
                        item_id: id,
                    });
                }
                Block::ToolCall {
                    name,
                    args,
                    call_id,
                    ..
                } => {
                    let shellish = matches!(
                        name.as_str(),
                        "exec" | "exec_command" | "bash" | "shell" | "sh" | "terminal"
                    );
                    let id = call_id.clone().unwrap_or_else(|| mint("call_", &mut n));
                    let payload = if shellish {
                        json!({
                            "type": "custom_tool_call",
                            "call_id": id,
                            "name": "exec",
                            "input": args,
                            "status": "completed",
                        })
                    } else {
                        json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args,
                        })
                    };
                    let command = shell_command(args, name);
                    pending.push((Some(id.clone()), name.clone(), command.clone()));
                    out.push(Item {
                        wrapper: "response_item",
                        payload,
                        thread: Some((
                            "commandExecution".into(),
                            json!({
                                "type": "commandExecution",
                                "id": id,
                                "command": command,
                                "cwd": session.cwd,
                                "status": "in_progress",
                                "aggregatedOutput": "",
                                "exitCode": null,
                                "durationMs": null,
                                "source": "xses-import",
                            }),
                        )),
                        item_id: id,
                    });
                }
                Block::ToolResult {
                    name,
                    output,
                    call_id,
                    is_error,
                    duration_ms,
                } => {
                    let id = call_id
                        .clone()
                        .or_else(|| {
                            pending
                                .iter()
                                .rev()
                                .find(|(c, n, _)| c.is_none() && n == name)
                                .and_then(|(c, _, _)| c.clone())
                        })
                        .unwrap_or_else(|| mint("call_", &mut n));
                    let shellish = pending
                        .iter()
                        .any(|(c, n, _)| n == name && (c.as_deref() == Some(&id) || c.is_none()));
                    let wrapper_payload = if shellish
                        && matches!(
                            name.as_str(),
                            "exec" | "exec_command" | "bash" | "shell" | "sh" | "terminal"
                        ) {
                        json!({
                            "type": "custom_tool_call_output",
                            "call_id": id,
                            "output": [{"type": "input_text", "text": output}],
                        })
                    } else {
                        json!({
                            "type": "function_call_output",
                            "call_id": id,
                            "output": output,
                        })
                    };
                    let command = pending
                        .iter()
                        .rev()
                        .find(|(c, _, _)| c.as_deref() == Some(&id))
                        .map(|(_, _, cmd)| cmd.clone())
                        .unwrap_or_else(|| name.clone());
                    out.push(Item {
                        wrapper: "response_item",
                        payload: wrapper_payload,
                        thread: Some((
                            "commandExecution".into(),
                            json!({
                                "type": "commandExecution",
                                "id": id,
                                "command": command,
                                "cwd": session.cwd,
                                "status": if *is_error { "failed" } else { "completed" },
                                "aggregatedOutput": output,
                                "exitCode": Value::Null,
                                "durationMs": duration_ms,
                                "source": "xses-import",
                            }),
                        )),
                        item_id: id,
                    });
                }
                Block::Image {
                    mime, data, path, ..
                } => {
                    let id = mint("img_", &mut n);
                    let payload = match data {
                        Some(d) => json!({
                            "type": "message",
                            "id": id,
                            "role": "user",
                            "content": [{
                                "type": "input_image",
                                "image_url": format!("data:{};base64,{d}", mime.clone().unwrap_or_else(|| "image/png".into())),
                            }],
                        }),
                        None => json!({
                            "type": "message",
                            "id": id,
                            "role": "user",
                            "content": [{"type": "input_text", "text": format!(
                                "[image reference: {}]",
                                path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| mime.clone().unwrap_or_else(|| "image".into()))
                            )}],
                        }),
                    };
                    let thread_text = path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| mime.clone().unwrap_or_else(|| "image".into()));
                    out.push(Item {
                        wrapper: "response_item",
                        payload,
                        thread: Some(match path {
                            Some(p) => (
                                "imageView".into(),
                                json!({"type": "imageView", "id": id, "path": p}),
                            ),
                            None => (
                                "agentMessage".into(),
                                json!({"type": "agentMessage", "id": id, "text": format!("[image: {thread_text}]")}),
                            ),
                        }),
                        item_id: id,
                    });
                }
                Block::Context { kind, text, path } => {
                    let id = mint("ctx_", &mut n);
                    let body = text
                        .clone()
                        .or_else(|| path.as_ref().map(|p| p.display().to_string()))
                        .unwrap_or_default();
                    let text = format!("[context: {kind}]\n{body}");
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "message",
                            "id": id,
                            "role": "developer",
                            "content": [{"type": "input_text", "text": text}],
                        }),
                        thread: Some((
                            "agentMessage".into(),
                            json!({"type": "agentMessage", "id": id, "text": text}),
                        )),
                        item_id: id,
                    });
                }
                Block::Patch { files, .. } => {
                    let id = mint("patch_", &mut n);
                    let text = format!(
                        "[patch: {} file{}]\n{}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        files
                            .iter()
                            .map(|f| f.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "message",
                            "id": id,
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        }),
                        thread: Some((
                            "agentMessage".into(),
                            json!({"type": "agentMessage", "id": id, "text": text}),
                        )),
                        item_id: id,
                    });
                }
                Block::Compressed { text } => {
                    let id = mint("cmp_", &mut n);
                    let text = format!("[compaction]\n{text}");
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "message",
                            "id": id,
                            "role": "system",
                            "content": [{"type": "input_text", "text": text}],
                        }),
                        thread: Some((
                            "contextCompaction".into(),
                            json!({"type": "contextCompaction", "id": id}),
                        )),
                        item_id: id,
                    });
                }
                Block::Usage {
                    input,
                    output,
                    reasoning,
                    cache_read,
                    cache_write,
                    ..
                } => {
                    let id = mint("usage_", &mut n);
                    out.push(Item {
                        wrapper: "event_msg",
                        payload: json!({
                            "type": "token_count",
                            "info": {
                                "last_token_usage": {
                                    "input_tokens": input,
                                    "cached_input_tokens": cache_read,
                                    "cache_write_input_tokens": cache_write,
                                    "output_tokens": output,
                                    "reasoning_output_tokens": reasoning,
                                    "total_tokens": input + output + reasoning,
                                },
                                "model_context_window": null,
                            },
                            "rate_limits": null,
                        }),
                        // The projection has no accounting row; skip it so the
                        // remaining ordinals still line up.
                        thread: None,
                        item_id: id,
                    });
                }
                Block::Gap { detail, .. } => {
                    let id = mint("gap_", &mut n);
                    let text = format!("[gap in imported history: {detail}]");
                    out.push(Item {
                        wrapper: "response_item",
                        payload: json!({
                            "type": "message",
                            "id": id,
                            "role": "system",
                            "content": [{"type": "input_text", "text": text}],
                        }),
                        thread: Some((
                            "agentMessage".into(),
                            json!({"type": "agentMessage", "id": id, "text": text}),
                        )),
                        item_id: id,
                    });
                }
            }
        }
    }
    out
}

/// The command a shell-ish tool actually ran, for the projection row.
fn shell_command(args: &str, fallback: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(args) {
        for key in ["command", "cmd", "script", "code"] {
            if let Some(s) = v.get(key).and_then(|c| c.as_str()) {
                return s.to_string();
            }
        }
    }
    if args.trim().is_empty() {
        fallback.to_string()
    } else {
        args.to_string()
    }
}

fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// Seed `thread_turns` + `thread_items` + projection state for an imported
/// rollout. Shapes mirror native rows (verified by diff): one turn per user
/// message, one projection item per rollout item, state pointing at
/// end-of-file so the recorder continues appending cleanly.
fn seed_codex_history(
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
    let mut stmts: Vec<String> = Vec::new();
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
        let first_user = items
            .get(*start)
            .map(|i| i.item_id.clone())
            .unwrap_or_default();
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_turns (thread_id, turn_id, rollout_ordinal, status, started_at, completed_at, duration_ms, first_user_item_id, final_agent_item_id, rollout_end_ordinal) VALUES ('{sid}', '{turn_id}', {ord}, 'completed', {ms}, {ms}, 0, '{first_user}', NULL, {ord});"
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
        let iid = &item.item_id;
        let item_sql = esc(&serde_json::to_string(thread_json).unwrap_or_default());
        stmts.push(format!(
            "INSERT OR REPLACE INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) VALUES ('{sid}', '{turn_id}', '{iid}', {ord}, {ms}, '{item_sql}', '{itype}');"
        ));
    }
    let next_ord = (items.len() + 2) as u64;
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
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            crate::issues::record(
                "codex-seed",
                sid,
                &format!(
                    "history seed failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
            false
        }
        Err(e) => {
            crate::issues::record("codex-seed", sid, &format!("wait failed: {e}"));
            false
        }
    }
}

/// Best-effort copy of the local codex account ids (from any native rollout).
/// Cached: the walk covers the whole sessions tree and each codex import
/// otherwise repeats it.
static CODEX_CREATOR_IDS: std::sync::LazyLock<Option<(String, String)>> =
    std::sync::LazyLock::new(|| dirs::home_dir().as_deref().and_then(find_creator_ids));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::Event;
    use crate::model::Tool;
    use chrono::Utc;

    fn test_session(events: Vec<Event>) -> CanonicalSession {
        CanonicalSession {
            id: "id".into(),
            tool: Tool::Omp,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            events,
        }
    }

    #[test]
    fn shell_calls_project_as_command_executions() {
        let s = test_session(vec![
            Event {
                msg: 0,
                block: Block::ToolCall {
                    name: "bash".into(),
                    args: r#"{"command":"ls -la"}"#.into(),
                    call_id: Some("c1".into()),
                    status: None,
                },
            },
            Event {
                msg: 1,
                block: Block::ToolResult {
                    name: "bash".into(),
                    output: "a\nb".into(),
                    call_id: Some("c1".into()),
                    is_error: false,
                    duration_ms: Some(12),
                },
            },
        ]);
        let items = rollout_items(&s);
        let types: Vec<&str> = items
            .iter()
            .map(|i| i.thread.as_ref().map(|(t, _)| t.as_str()).unwrap_or("-"))
            .collect();
        assert!(types.contains(&"commandExecution"));
        let ce = items
            .iter()
            .filter_map(|i| i.thread.as_ref())
            .find(|(t, j)| t == "commandExecution" && j["aggregatedOutput"] == "a\nb")
            .expect("completed execution row");
        assert_eq!(ce.1["status"], "completed");
        assert_eq!(ce.1["durationMs"], 12);
    }

    #[test]
    fn usage_skips_the_projection() {
        let s = test_session(vec![Event {
            msg: 0,
            block: Block::Usage {
                input: 1,
                output: 2,
                reasoning: 0,
                cache_read: 0,
                cache_write: 0,
                cost: 0.0,
                model: None,
            },
        }]);
        let items = rollout_items(&s);
        assert_eq!(items.len(), 2); // note + usage
        assert_eq!(items[1].thread, None);
        assert_eq!(items[1].wrapper, "event_msg");
    }
}
