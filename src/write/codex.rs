//! codex writer: rollout file + `threads` row + history projections.
//!
//! The rollout carries codex's own item vocabulary — `message` with
//! `input_text`/`output_text`/`input_image`, `reasoning`, `function_call`,
//! `function_call_output`, `custom_tool_call(_output)`, and `token_count`
//! events — while the TUI projection (`thread_items`) uses the item types the
//! picker renders: `userMessage`, `agentMessage`, `commandExecution`,
//! `contextCompaction`.
use super::{iso, iso_stamp, manifest, push, synth_id};
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
    /// The source message's own time, when it recorded one.
    ts: Option<chrono::DateTime<chrono::Utc>>,
}

pub(super) fn write(session: &CanonicalSession, revision: Option<&str>) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sid = synth_id(session.tool, &super::artifact_key(session, revision));
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
        // Each record carries the source message's own time when there is one,
        // so a resumed session shows the timeline it actually had.
        let item_ts = item.ts.map(|t| iso(&t));
        push(
            &mut buf,
            &json!({
                "ordinal": i + 2,
                "type": item.wrapper,
                "payload": item.payload,
                "timestamp": item_ts.unwrap_or_else(|| ts.clone()),
            }),
        );
    }
    if !super::atomic_write_journaled(
        &target,
        &buf,
        &format!("{} {}", session.tool.as_str(), session.id),
    ) {
        return None;
    }
    // codex's own picker lists from the `threads` table, not the sessions dir:
    // register the drop-in file so `codex resume` sees it.
    let title = super::title_for(session);
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
            ts: None,
        });
    }

    // Pending shell calls, so a result can complete its item.
    let mut pending: Vec<(Option<String>, String, ThreadTool)> = Vec::new();
    let mut ids = super::TargetIds::default();
    let mut prev_ts = session.created;
    for (m, blocks) in session.grouped() {
        let group_ts = match session.msg_times.get(&m).copied() {
            Some(ts) if ts > prev_ts => ts,
            _ => prev_ts + chrono::Duration::milliseconds(1),
        };
        prev_ts = group_ts;
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                    let id = call_id
                        .clone()
                        .map(|c| ids.get(&c))
                        .unwrap_or_else(|| mint("call_", &mut n));
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
                    let tool = classify_tool(name, args);
                    pending.push((Some(id.clone()), name.clone(), tool.clone()));
                    let thread = match &tool {
                        ThreadTool::Shell(command) => (
                            "commandExecution",
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
                        ),
                        // codex's TUI draws a patch as a file change, not as a
                        // command; the diff lives in the call's arguments.
                        ThreadTool::FileChange(changes) => (
                            "fileChange",
                            json!({"type": "fileChange", "id": id, "changes": changes}),
                        ),
                        ThreadTool::Mcp { server, tool } => (
                            "mcpToolCall",
                            json!({
                                "type": "mcpToolCall",
                                "id": id,
                                "server": server,
                                "tool": tool,
                                "status": "in_progress",
                                "arguments": serde_json::from_str::<Value>(args).unwrap_or(Value::Null),
                                "result": null,
                            }),
                        ),
                        ThreadTool::ImageView(path) => (
                            "imageView",
                            json!({"type": "imageView", "id": id, "path": path}),
                        ),
                    };
                    out.push(Item {
                        wrapper: "response_item",
                        payload,
                        thread: Some((thread.0.to_string(), thread.1)),
                        item_id: id,
                        ts: Some(group_ts),
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
                    let tool = pending
                        .iter()
                        .rev()
                        .find(|(c, n, _)| *n == *name && (c.as_deref() == Some(&id) || c.is_none()))
                        .map(|(_, _, t)| t.clone())
                        .unwrap_or_else(|| classify_tool(name, ""));
                    let command = match &tool {
                        ThreadTool::Shell(command) => command.clone(),
                        _ => name.clone(),
                    };
                    // A successful patch already rendered as the file change
                    // itself; only its failure needs a row of its own.
                    let thread = match &tool {
                        ThreadTool::FileChange(_) if !*is_error => None,
                        ThreadTool::ImageView(_) => None,
                        ThreadTool::Mcp { server, tool: mcp } => Some((
                            "mcpToolCall".to_string(),
                            json!({
                                "type": "mcpToolCall",
                                "id": id,
                                "server": server,
                                "tool": mcp,
                                "status": if *is_error { "failed" } else { "completed" },
                                "arguments": Value::Null,
                                "result": {"content": [{"type": "text", "text": output}]},
                            }),
                        )),
                        _ => Some((
                            "commandExecution".to_string(),
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
                    };
                    out.push(Item {
                        wrapper: "response_item",
                        payload: wrapper_payload,
                        thread,
                        item_id: id,
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
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
                        ts: Some(group_ts),
                    });
                }
            }
        }
    }
    out
}

/// The command a shell-ish tool actually ran, for the projection row.
#[derive(Clone)]
/// How a tool call shows up in codex's own transcript projection. codex only
/// renders a shell string for `commandExecution`; edits, MCP calls and image
/// views have their own item types, which is what its TUI draws.
enum ThreadTool {
    Shell(String),
    FileChange(Vec<Value>),
    Mcp { server: String, tool: String },
    ImageView(PathBuf),
}

fn classify_tool(name: &str, args: &str) -> ThreadTool {
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or(("mcp", rest));
        return ThreadTool::Mcp {
            server: name
                .get("mcp__".len()..)
                .and_then(|r| r.split_once("__").map(|(s, _)| s))
                .unwrap_or(server)
                .to_string(),
            tool: name
                .rsplit_once("__")
                .map(|(_, t)| t)
                .unwrap_or(tool)
                .to_string(),
        };
    }
    let parsed: Option<Value> = serde_json::from_str(args).ok();
    let field = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            parsed
                .as_ref()
                .and_then(|v| v.get(*k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
    };
    if matches!(
        lower.as_str(),
        "view_image" | "viewimage" | "image_view" | "read_image"
    ) {
        if let Some(path) = field(&["file_path", "path", "filePath", "filename"]) {
            return ThreadTool::ImageView(PathBuf::from(path));
        }
    }
    if lower == "apply_patch" || lower == "patch" {
        let text = field(&["patch", "input", "content"]).unwrap_or_else(|| args.to_string());
        let changes = patch_changes(&text);
        if !changes.is_empty() {
            return ThreadTool::FileChange(changes);
        }
    }
    if matches!(
        lower.as_str(),
        "write"
            | "create_file"
            | "save_file"
            | "edit"
            | "multiedit"
            | "str_replace"
            | "str_replace_editor"
            | "notebookedit"
            | "notebook_edit"
    ) {
        if let Some(path) = field(&["file_path", "path", "filePath"]) {
            let diff = field(&["content", "new_string", "new_str"]).unwrap_or_default();
            // Whether the file was created or rewritten is not knowable from
            // the call alone, so the change is an update; codex's own patches
            // report `add` only because it applies them itself.
            return ThreadTool::FileChange(vec![json!({
                "path": path,
                "kind": {"type": "update"},
                "diff": diff,
            })]);
        }
    }
    ThreadTool::Shell(shell_command(args, name))
}

/// Paths, kinds and bodies from a codex `apply_patch` body.
fn patch_changes(text: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    let flush = |out: &mut Vec<Value>, cur: Option<(String, String, Vec<String>)>| {
        if let Some((path, kind, lines)) = cur {
            out.push(json!({
                "path": path,
                "kind": {"type": kind},
                "diff": lines.join("\n"),
            }));
        }
    };
    for line in text.lines() {
        let marker = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|m| line.strip_prefix(m).map(|rest| (*m, rest.trim())));
        match marker {
            Some((m, path)) => {
                flush(&mut out, current.take());
                let kind = match m {
                    "*** Add File: " => "add",
                    "*** Delete File: " => "delete",
                    _ => "update",
                };
                current = Some((path.to_string(), kind.to_string(), Vec::new()));
            }
            None => {
                if let Some((_, _, lines)) = current.as_mut() {
                    lines.push(line.to_string());
                }
            }
        }
    }
    flush(&mut out, current.take());
    out
}

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
    let script = "BEGIN IMMEDIATE;\n".to_owned() + &stmts.join(";\n") + ";\nCOMMIT;\n";
    let mut command = std::process::Command::new("sqlite3");
    command.args(["-bail", "-cmd", ".timeout 5000"]).arg(&db);
    match crate::proc::output_with_input(
        command,
        script.into_bytes(),
        std::time::Duration::from_secs(30),
    ) {
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

/// Remove an obsolete generation's picker and projection entries. The index
/// retains failed retirements, so a locked database is retried on the next sync.
pub(crate) fn retire_codex(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let home = dirs::home_dir().context("no home dir")?;
    let name = path
        .file_stem()
        .and_then(|p| p.to_str())
        .context("invalid rollout path")?;
    let sid = name
        .get(name.len().saturating_sub(36)..)
        .context("invalid rollout id")?;
    let sid = sid.replace('\'', "''");
    let mut queries = Vec::new();
    if let Some(db) = codex_state_db(&home) {
        queries.push((db, format!("DELETE FROM threads WHERE id='{sid}';")));
    }
    let history = home.join(".codex/thread_history_1.sqlite");
    if history.exists() {
        queries.push((history, format!("BEGIN IMMEDIATE; DELETE FROM thread_items WHERE thread_id='{sid}'; DELETE FROM thread_turns WHERE thread_id='{sid}'; DELETE FROM thread_history_projection_state WHERE thread_id='{sid}'; COMMIT;")));
    }
    for (db, sql) in queries {
        let mut command = std::process::Command::new("sqlite3");
        command.args(["-cmd", ".timeout 5000"]).arg(db).arg(sql);
        let out = crate::proc::output_with_timeout(command, std::time::Duration::from_secs(10))?;
        anyhow::ensure!(
            out.status.success(),
            "Codex retirement failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
            title: None,
            events,
            msg_times: Default::default(),
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
    fn tool_calls_project_as_codex_native_items() {
        let s = test_session(vec![
            Event {
                msg: 0,
                block: Block::ToolCall {
                    name: "apply_patch".into(),
                    args: r#"{"patch":"*** Begin Patch\n*** Update File: /tmp/a.py\n@@\n-x = 1\n+x = 2\n*** Add File: /tmp/b.py\n+print(1)\n*** End Patch"}"#.into(),
                    call_id: Some("c1".into()),
                    status: None,
                },
            },
            Event {
                msg: 1,
                block: Block::ToolResult {
                    name: "apply_patch".into(),
                    output: "Done!".into(),
                    call_id: Some("c1".into()),
                    is_error: false,
                    duration_ms: None,
                },
            },
            Event {
                msg: 2,
                block: Block::ToolCall {
                    name: "mcp__cua_repl__js".into(),
                    args: r#"{"code":"1+1"}"#.into(),
                    call_id: Some("c2".into()),
                    status: None,
                },
            },
            Event {
                msg: 3,
                block: Block::ToolResult {
                    name: "mcp__cua_repl__js".into(),
                    output: "2".into(),
                    call_id: Some("c2".into()),
                    is_error: false,
                    duration_ms: None,
                },
            },
            Event {
                msg: 4,
                block: Block::ToolCall {
                    name: "view_image".into(),
                    args: r#"{"path":"/tmp/shot.png"}"#.into(),
                    call_id: Some("c3".into()),
                    status: None,
                },
            },
            Event {
                msg: 5,
                block: Block::ToolCall {
                    name: "bash".into(),
                    args: r#"{"command":"ls"}"#.into(),
                    call_id: Some("c4".into()),
                    status: None,
                },
            },
        ]);
        let items = rollout_items(&s);
        let by_type = |t: &str| -> Vec<&Value> {
            items
                .iter()
                .filter_map(|i| i.thread.as_ref())
                .filter(|(kind, _)| kind == t)
                .map(|(_, j)| j)
                .collect()
        };
        // A patch is a file change with both files, kinds and bodies.
        let changes = &by_type("fileChange")[0]["changes"];
        assert_eq!(changes.as_array().map(Vec::len), Some(2), "{changes}");
        assert_eq!(changes[0]["kind"]["type"], "update");
        assert_eq!(changes[0]["path"], "/tmp/a.py");
        assert!(changes[0]["diff"].as_str().unwrap_or("").contains("+x = 2"));
        assert_eq!(changes[1]["kind"]["type"], "add");
        assert_eq!(changes[1]["path"], "/tmp/b.py");
        // Its successful result adds nothing; the diff is the result.
        assert_eq!(by_type("fileChange").len(), 1);
        assert_eq!(by_type("commandExecution").len(), 1, "only the shell call");

        // MCP: server, tool, arguments at the call, status and output after.
        let mcp = by_type("mcpToolCall");
        assert_eq!(mcp.len(), 2, "call row plus completed row");
        assert_eq!(mcp[0]["server"], "cua_repl");
        assert_eq!(mcp[0]["tool"], "js");
        assert_eq!(mcp[0]["arguments"]["code"], "1+1");
        assert_eq!(mcp[1]["status"], "completed");
        assert_eq!(mcp[1]["result"]["content"][0]["text"], "2");

        // An image view keeps codex's own item type and path.
        let view = by_type("imageView");
        assert_eq!(view[0]["path"], "/tmp/shot.png");
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
