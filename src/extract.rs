//! Extract: source transcript → IR (`CanonicalSession`).
//!
//! One extractor per source tool. Sources are strictly read-only. Everything a
//! source holds is captured: tool calls and results with their correlation ids,
//! reasoning (with its signature when present), injected context, images,
//! patches, compaction summaries, and token/cost accounting. Anything that
//! cannot be captured becomes a named gap rather than disappearing.
use crate::canonical::{Block, Block::*, CanonicalSession, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Largest binary payload carried inline. Sources embed base64 images; past
/// this size a sync would bloat every target store, so the image is recorded
/// as a gap and the manifest says why.
const MAX_INLINE_BYTES: u64 = 5 * 1024 * 1024;

pub fn extract(
    tool: Tool,
    id: &str,
    cwd: &Path,
    created: chrono::DateTime<chrono::Utc>,
    path: &Path,
) -> Result<CanonicalSession> {
    let collected = match tool {
        Tool::Claude => claude(path)?,
        Tool::Codex => codex(path)?,
        Tool::Pi => pi_family(path, Tool::Pi)?,
        Tool::Omp => pi_family(path, Tool::Omp)?,
        Tool::Opencode => opencode(id)?,
    };
    let (events, msg_times) = collected.into_parts();
    Ok(CanonicalSession {
        id: id.to_string(),
        tool,
        cwd: cwd.to_path_buf(),
        created,
        title: None,
        events,
        msg_times,
    })
}

/// opencode transcripts via read-only SQL (message + part `data` JSON).
/// The runtime shapes are identical to `opencode export` output, but SQL is
/// deterministic: the CLI truncates large stdout to 64KB multiples with exit 0.
fn opencode(id: &str) -> Result<Events> {
    let Some(db) = crate::scan::opencode_db() else {
        anyhow::bail!("opencode database not found")
    };
    let esc = id.replace('\'', "''");
    let run = |sql: &str| -> Result<Vec<serde_json::Value>> {
        let q = std::process::Command::new("sqlite3")
            .arg("-readonly")
            .arg("-json")
            .arg(&db)
            .arg(sql)
            .output()?;
        if !q.status.success() {
            anyhow::bail!(
                "sqlite read failed: {}",
                String::from_utf8_lossy(&q.stderr).trim()
            );
        }
        Ok(
            crate::proc::json_rows(&String::from_utf8_lossy(&q.stdout), "opencode transcript")
                .unwrap_or_default(),
        )
    };
    let msgs = run(&format!(
        "SELECT id, data FROM message WHERE session_id = '{esc}' ORDER BY time_created, id;"
    ))?;
    let parts = run(&format!(
        "SELECT data, message_id FROM part WHERE session_id = '{esc}' ORDER BY time_created, id;"
    ))?;
    use std::collections::HashMap;
    let mut by_msg: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for p in parts {
        if let (Some(mid), Some(data)) = (
            p.get("message_id").and_then(|m| m.as_str()),
            p.get("data")
                .and_then(|d| d.as_str())
                .and_then(|s| serde_json::from_str(s).ok()),
        ) {
            by_msg.entry(mid.to_string()).or_default().push(data);
        }
    }
    let mut out = Events::default();
    for m in msgs {
        let Some(minfo) = m
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        else {
            continue;
        };
        let role = match minfo.get("role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("system") => Role::System,
            _ => Role::Assistant,
        };
        let mid = m.get("id").and_then(|i| i.as_str()).unwrap_or("");
        out.boundary();
        // opencode stores the message time in ms.
        if let Some(ts) = ms_time(&m) {
            out.at(ts);
        }
        if let Some(items) = by_msg.get(mid) {
            for part in items {
                opencode_part(part, role, &mut out);
            }
        }
    }
    Ok(out)
}

/// One opencode part → blocks. Kept separate from the message walk so the part
/// vocabulary can be exercised directly.
fn opencode_part(part: &Value, role: Role, out: &mut Events) {
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = classify_text(part.get("text")) {
                out.push(Text { role, text: t });
            }
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    out.push(Thinking {
                        text: t.to_string(),
                        signature: None,
                    });
                }
            }
        }
        Some("tool") => {
            let name = part
                .get("tool")
                .and_then(|t| t.as_str())
                .unwrap_or("tool")
                .to_string();
            let call_id = part
                .get("callID")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            let state = part.get("state").cloned().unwrap_or(Value::Null);
            let status = state
                .get("status")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            let input = state
                .get("input")
                .map(|i| i.to_string())
                .unwrap_or_default();
            out.push(ToolCall {
                name: name.clone(),
                args: input,
                call_id: call_id.clone(),
                status: status.clone(),
            });
            let output = state
                .get("output")
                .and_then(|o| o.as_str())
                .map(str::to_string);
            let error = state
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_string);
            let duration = state
                .pointer("/time/end")
                .and_then(|e| e.as_i64())
                .zip(state.pointer("/time/start").and_then(|s| s.as_i64()))
                .map(|(end, start)| (end - start).max(0) as u64);
            match (output, error) {
                (Some(o), _) if !o.is_empty() => out.push(ToolResult {
                    name: name.clone(),
                    output: o,
                    call_id: call_id.clone(),
                    is_error: status.as_deref() == Some("error"),
                    duration_ms: duration,
                }),
                (_, Some(e)) if !e.is_empty() => out.push(ToolResult {
                    name: name.clone(),
                    output: e,
                    call_id: call_id.clone(),
                    is_error: true,
                    duration_ms: duration,
                }),
                _ => out.push(tool_output_gap(&name)),
            }
        }
        Some("patch") => out.push(Patch {
            hash: part
                .get("hash")
                .and_then(|h| h.as_str())
                .map(str::to_string),
            files: part
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        Some("compaction") => out.push(Compressed {
            text: "session was compacted; earlier history summarized away".into(),
        }),
        Some("file") => {
            // Attachments are `file` parts here; the image ones
            // carry a data URL and must transfer as images rather
            // than as a context note with nothing in it.
            let mime = part.get("mime").and_then(|m| m.as_str()).unwrap_or("");
            let url = part.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if mime.starts_with("image/") || url.starts_with("data:image/") {
                out.push(image_block(part, Tool::Opencode));
                return;
            }
            let path = part
                .get("path")
                .or_else(|| part.pointer("/file/path"))
                .and_then(|p| p.as_str())
                .map(PathBuf::from);
            let text = part
                .get("text")
                .and_then(|t| t.as_str())
                .map(str::to_string);
            out.push(Context {
                kind: "file".into(),
                text,
                path,
            });
        }
        Some("subtask") => out.push(Context {
            kind: "subtask".into(),
            text: part
                .get("description")
                .and_then(|d| d.as_str())
                .map(|d| format!("subtask: {d}")),
            path: None,
        }),
        Some("step-finish") => {
            let t = part.get("tokens").cloned().unwrap_or(Value::Null);
            out.push(Usage {
                input: num(&t, "/input"),
                output: num(&t, "/output"),
                reasoning: num(&t, "/reasoning"),
                cache_read: num(&t, "/cache/read"),
                cache_write: num(&t, "/cache/write"),
                cost: part.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0),
                model: None,
            });
        }
        // step-start/agent parts are run bookkeeping, not content.
        Some("step-start" | "agent") => {}
        Some(other) => out.push(Gap {
            kind: "opencode-part".into(),
            detail: format!("opencode {other} part not carried over"),
        }),
        None => {}
    }
}

/// A source row's own timestamp, when it records one.
fn row_time(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// opencode keeps milliseconds since the epoch on the message, not a string.
fn ms_time(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let ms = v
        .pointer("/time/created")
        .or_else(|| v.get("time_created"))
        .and_then(|t| t.as_i64())?;
    chrono::DateTime::from_timestamp_millis(ms)
}

fn num(v: &Value, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(|n| n.as_u64()).unwrap_or(0)
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    Ok(BufReader::new(fs::File::open(path)?).lines())
}

/// Parse one JSONL record. A truncated or corrupt line (partial write from a
/// crashed agent) becomes a visible gap instead of failing the whole session.
fn parse_record(line_no: usize, line: &str, out: &mut Events) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => Some(v),
        Err(_) => {
            out.push(Gap {
                kind: "unreadable-record".into(),
                detail: format!("record at line {} could not be read", line_no + 1),
            });
            None
        }
    }
}

fn claude(path: &Path) -> Result<Events> {
    let mut out = Events::default();
    // Dedupes repeated meta injections (claude re-sends the same skill body on
    // every turn it applies to).
    let mut seen_meta: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = parse_record(line_no, &line, &mut out) else {
            continue;
        };
        let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Sidechain (subagent) rows are a different conversation: carrying
        // them would present someone else's turns as this transcript's, so
        // they stay in the source.
        if v.get("isSidechain")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        // Meta rows are injections claude made itself (skill bodies, local
        // command output). They are context the model read, so they transfer
        // as context — once: the same injection repeats on every later turn,
        // and one copy is the whole information.
        if v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false) {
            if let Some(text) = v.pointer("/message/content").and_then(text_of) {
                let text = text.trim();
                if !text.is_empty() && seen_meta.insert(text.to_string()) {
                    out.boundary();
                    if let Some(ts) = row_time(&v) {
                        out.at(ts);
                    }
                    out.push(Context {
                        kind: "meta".into(),
                        text: Some(text.to_string()),
                        path: None,
                    });
                }
            }
            continue;
        }
        match t {
            // A compact summary rides as a user row; it is the compaction, not
            // something the user typed.
            "user"
                if v.get("isCompactSummary")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false) =>
            {
                out.boundary();
                if let Some(ts) = row_time(&v) {
                    out.at(ts);
                }
                let summary = v
                    .pointer("/message/content")
                    .and_then(text_of)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "earlier history summarized away".into());
                out.push(Compressed {
                    text: format!("session was compacted; summary follows\n{summary}"),
                });
            }
            "user" | "assistant" | "system" => {
                let role = match t {
                    "user" => Role::User,
                    "system" => Role::System,
                    _ => Role::Assistant,
                };
                out.boundary();
                if let Some(ts) = row_time(&v) {
                    out.at(ts);
                }
                if let Some(content) = v.pointer("/message/content") {
                    collect_parts(content, role, Tool::Claude, &mut out);
                }
                if let Some(u) = v.pointer("/message/usage") {
                    out.push(usage_from_claude(u));
                }
            }
            "attachment" => {
                if let Some(block) = claude_attachment(v.get("attachment")) {
                    out.push(block);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn usage_from_claude(u: &Value) -> Block {
    Usage {
        input: num(u, "/input_tokens"),
        output: num(u, "/output_tokens"),
        reasoning: num(u, "/output_tokens_details/thinking_tokens"),
        cache_read: num(u, "/cache_read_input_tokens"),
        cache_write: num(u, "/cache_creation_input_tokens"),
        cost: 0.0,
        model: None,
    }
}

/// Content-bearing claude attachment → context block; environment noise is
/// skipped (it is regenerated by every agent on resume).
fn claude_attachment(a: Option<&Value>) -> Option<Block> {
    let a = a?;
    let path = a
        .get("filename")
        .and_then(|f| f.as_str())
        .map(PathBuf::from);
    match a.get("type").and_then(|t| t.as_str()) {
        // claude embeds what it read: text (with the line range) or an image
        // payload. Carrying that beats a note that says it was not carried.
        Some("file") => Some(attachment_content(a, path)),
        Some("compact_file_reference") => Some(if a.get("content").is_some() {
            // Embedded content is evidence: carry it whatever the reference is.
            attachment_content(a, path.clone())
        } else {
            match attachment_content(a, path.clone()) {
                // A readable image on disk still transfers.
                image @ Image { .. } => image,
                // Otherwise the reference is all there is: say what it means,
                // so the resumed agent knows it can read the file itself.
                _ => Context {
                    kind: "pre-compaction-reference".into(),
                    text: Some("read before the last compaction; re-read if needed".into()),
                    path,
                },
            }
        }),
        Some("thinking_stripped") => Some(Gap {
            kind: "thinking".into(),
            detail: "reasoning stripped before import; not carried over".into(),
        }),
        Some("queued_command") => {
            let status = a
                .get("prompt")
                .and_then(|p| p.as_str())
                .and_then(|p| p.split("<status>").nth(1)?.split("</status>").next())
                .unwrap_or("event");
            Some(Context {
                kind: "background-task".into(),
                text: Some(format!("background task {status}; output not carried over")),
                path: None,
            })
        }
        _ => None,
    }
}

fn codex(path: &Path) -> Result<Events> {
    let mut out = Events::default();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = parse_record(line_no, &line, &mut out) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response_item") => {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                match payload.get("type").and_then(|t| t.as_str()) {
                    Some("message") => {
                        let role = match payload.get("role").and_then(|r| r.as_str()) {
                            Some("user") => Role::User,
                            Some("system") => Role::System,
                            _ => Role::Assistant,
                        };
                        out.boundary();
                        if let Some(ts) = row_time(&v) {
                            out.at(ts);
                        }
                        if let Some(c) = payload.get("content") {
                            collect_parts(c, role, Tool::Codex, &mut out);
                        }
                    }
                    Some("reasoning") => {
                        out.boundary();
                        if let Some(ts) = row_time(&v) {
                            out.at(ts);
                        }
                        let text = payload
                            .get("content")
                            .and_then(|c| c.as_str())
                            .filter(|t| !t.trim().is_empty())
                            .map(str::to_string)
                            .or_else(|| {
                                payload
                                    .pointer("/summary/0/text")
                                    .and_then(|t| t.as_str())
                                    .filter(|t| !t.trim().is_empty())
                                    .map(str::to_string)
                            });
                        match text {
                            Some(t) => out.push(Thinking {
                                text: t,
                                signature: None,
                            }),
                            None => out.push(Gap {
                                kind: "thinking".into(),
                                detail: "reasoning was not stored in plain text".into(),
                            }),
                        }
                    }
                    Some("function_call") => {
                        out.boundary();
                        if let Some(ts) = row_time(&v) {
                            out.at(ts);
                        }
                        out.push(ToolCall {
                            name: payload
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: payload
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("")
                                .to_string(),
                            call_id: payload
                                .get("call_id")
                                .or_else(|| payload.get("id"))
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            status: payload
                                .get("status")
                                .and_then(|s| s.as_str())
                                .map(str::to_string),
                        });
                    }
                    Some("function_call_output") => {
                        out.push(ToolResult {
                            name: "function".into(),
                            output: payload
                                .get("output")
                                .map(|o| o.to_string())
                                .unwrap_or_default(),
                            call_id: payload
                                .get("call_id")
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            is_error: false,
                            duration_ms: None,
                        });
                    }
                    Some("custom_tool_call") => {
                        out.boundary();
                        if let Some(ts) = row_time(&v) {
                            out.at(ts);
                        }
                        out.push(ToolCall {
                            name: payload
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("exec")
                                .to_string(),
                            args: payload
                                .get("input")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string(),
                            call_id: payload
                                .get("call_id")
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            status: payload
                                .get("status")
                                .and_then(|s| s.as_str())
                                .map(str::to_string),
                        });
                    }
                    Some("custom_tool_call_output") => {
                        out.push(ToolResult {
                            name: "exec".into(),
                            output: payload.get("output").and_then(text_of).unwrap_or_default(),
                            call_id: payload
                                .get("call_id")
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            is_error: false,
                            duration_ms: None,
                        });
                    }
                    Some(other) => out.push(Gap {
                        kind: "codex-item".into(),
                        detail: format!("codex {other} item not carried over"),
                    }),
                    None => {}
                }
            }
            // Lifecycle events carry accounting and free text; item content
            // mirrors the response_items above.
            // codex records compaction as its own row (`compacted`), whose
            // `replacement_history` is the retained context codex keeps
            // sending. The conversation continues in later response_items, so
            // only the compaction itself is carried.
            Some("compacted") => {
                out.boundary();
                if let Some(ts) = row_time(&v) {
                    out.at(ts);
                }
                out.push(codex_compaction(
                    &v.get("payload").cloned().unwrap_or(Value::Null),
                ));
            }
            Some("event_msg") => {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                match payload.get("type").and_then(|t| t.as_str()) {
                    Some("token_count") => {
                        if let Some(u) = payload.pointer("/info/last_token_usage") {
                            out.push(Usage {
                                input: num(u, "/input_tokens"),
                                output: num(u, "/output_tokens"),
                                reasoning: num(u, "/reasoning_output_tokens"),
                                cache_read: num(u, "/cached_input_tokens"),
                                cache_write: num(u, "/cache_write_input_tokens"),
                                cost: 0.0,
                                model: None,
                            });
                        }
                    }
                    _ => {
                        if let Some(t) = payload.get("text").and_then(|t| t.as_str()) {
                            if !t.trim().is_empty() {
                                out.push(Text {
                                    role: Role::System,
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn pi_family(path: &Path, tool: Tool) -> Result<Events> {
    let mut out = Events::default();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = parse_record(line_no, &line, &mut out) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                let msg = v.get("message").cloned().unwrap_or(Value::Null);
                let msg_role = msg.get("role").and_then(|r| r.as_str());
                let Some(content) = msg.get("content") else {
                    continue;
                };
                out.boundary();
                if let Some(ts) = row_time(&v) {
                    out.at(ts);
                }
                match msg_role {
                    Some("toolResult") => {
                        let name = msg
                            .get("toolName")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let call_id = msg
                            .get("toolCallId")
                            .and_then(|c| c.as_str())
                            .map(str::to_string);
                        let is_error = msg
                            .get("isError")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false);
                        let duration = msg.get("durationMs").and_then(|d| d.as_u64());
                        for it in content.as_array().cloned().unwrap_or_default() {
                            if let Some("image") = it.get("type").and_then(|t| t.as_str()) {
                                out.push(image_block(&it, tool));
                            }
                        }
                        let output = text_of(content).unwrap_or_default();
                        if !output.trim().is_empty() {
                            out.push(ToolResult {
                                name,
                                output,
                                call_id,
                                is_error,
                                duration_ms: duration,
                            });
                        } else if !content_has_image(content) {
                            out.push(tool_output_gap(&name));
                        }
                    }
                    _ => {
                        let role = match msg_role {
                            Some("user") => Role::User,
                            Some("system") | Some("developer") => Role::System,
                            _ => Role::Assistant,
                        };
                        collect_parts(content, role, tool, &mut out);
                        if let Some(u) = msg.get("usage") {
                            out.push(Usage {
                                input: num(u, "/input"),
                                output: num(u, "/output"),
                                reasoning: num(u, "/reasoning"),
                                cache_read: num(u, "/cacheRead"),
                                cache_write: num(u, "/cacheWrite"),
                                cost: u
                                    .pointer("/cost/total")
                                    .and_then(|c| c.as_f64())
                                    .unwrap_or(0.0),
                                model: msg
                                    .get("model")
                                    .and_then(|m| m.as_str())
                                    .map(str::to_string),
                            });
                        }
                    }
                }
            }
            Some("compaction") => {
                let summary = v
                    .get("summary")
                    .or_else(|| v.get("shortSummary"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("session compacted");
                out.boundary();
                out.push(Compressed {
                    text: summary.to_string(),
                });
            }
            Some("context_edit") => out.push(Context {
                kind: "context-edit".into(),
                text: Some(format!(
                    "context {} for {}",
                    if v.get("replacement").map(|r| !r.is_null()).unwrap_or(false) {
                        "replaced"
                    } else {
                        "omitted"
                    },
                    v.get("targetId").and_then(|t| t.as_str()).unwrap_or("?")
                )),
                path: None,
            }),
            Some("ttsr_injection") | Some("custom_message") => {
                let text = v
                    .get("content")
                    .or_else(|| v.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string);
                out.push(Context {
                    kind: v
                        .get("customType")
                        .and_then(|c| c.as_str())
                        .unwrap_or("injection")
                        .to_string(),
                    text,
                    path: None,
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

fn content_has_image(content: &Value) -> bool {
    content.as_array().is_some_and(|items| {
        items.iter().any(|it| {
            matches!(
                it.get("type").and_then(|t| t.as_str()),
                Some("image" | "input_image")
            )
        })
    })
}

/// codex's compaction row → `Compressed`. Its `message` is usually empty, so
/// the retained history's size is what tells the reader how much went away.
fn codex_compaction(payload: &Value) -> Block {
    let text = payload
        .get("message")
        .and_then(|m| m.as_str())
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let retained = payload
                .get("replacement_history")
                .and_then(|h| h.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            format!("session was compacted; {retained} retained items summarized away")
        });
    Compressed { text }
}

/// What an attachment holds: embedded text or image, an on-disk image, or a
/// note saying precisely what was dropped.
fn attachment_content(a: &Value, path: Option<PathBuf>) -> Block {
    let display = a
        .get("displayPath")
        .and_then(|d| d.as_str())
        .map(str::to_string)
        .or_else(|| {
            a.get("filename")
                .and_then(|f| f.as_str())
                .map(|f| f.rsplit('/').next().unwrap_or(f).to_string())
        })
        .unwrap_or_else(|| "file".into());
    let embedded = a.get("content");
    // An image payload: same shape the transcript uses elsewhere.
    if let Some(content) = embedded {
        let kind = content.get("type").and_then(|t| t.as_str());
        if kind == Some("image") || content.get("source").is_some() {
            return image_block(content, Tool::Claude);
        }
    }
    // Text payload: `{type: text, file: {content, startLine, totalLines}}`.
    let file = embedded.and_then(|c| c.get("file"));
    let text = file
        .and_then(|f| f.get("content"))
        .and_then(|c| c.as_str())
        .filter(|t| !t.trim().is_empty());
    if let Some(text) = text {
        let start = file
            .and_then(|f| f.get("startLine"))
            .and_then(|l| l.as_u64());
        let total = file
            .and_then(|f| f.get("totalLines"))
            .and_then(|l| l.as_u64());
        let range = match (start, total) {
            (Some(s), Some(t)) if s > 1 => format!(" (lines {s}-{t})"),
            (_, Some(t)) => format!(" ({t} lines)"),
            _ => String::new(),
        };
        return Context {
            kind: "attachment".into(),
            text: Some(format!("attached {display}{range}\n{text}")),
            path,
        };
    }
    // Nothing embedded: an image on disk still transfers, other files do not.
    let Some(path) = path else {
        return Context {
            kind: "attachment".into(),
            text: Some(format!("attached {display} not carried")),
            path: None,
        };
    };
    let mime = mime_for_path(&path);
    match (mime, fs::metadata(&path)) {
        (Some(mime), Ok(meta)) if meta.is_file() && meta.len() <= MAX_INLINE_BYTES => {
            match fs::read(&path) {
                Ok(bytes) => Image {
                    mime: Some(mime.into()),
                    data: Some(base64_encode(&bytes)),
                    path: Some(path),
                    bytes: meta.len(),
                },
                Err(_) => Context {
                    kind: "attachment".into(),
                    text: Some(format!("attached {display} not carried (unreadable)")),
                    path: Some(path),
                },
            }
        }
        (Some(_), Ok(meta)) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached image {display} not carried ({:.1} MB exceeds the {} MB inline cap)",
                meta.len() as f64 / 1e6,
                MAX_INLINE_BYTES / 1024 / 1024
            )),
            path: Some(path),
        },
        (Some(_), Err(_)) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached image {display} not carried (file is gone)"
            )),
            path: Some(path),
        },
        (None, _) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached file {display} not carried (only images transfer by path)"
            )),
            path: Some(path),
        },
    }
}

/// Image mime for a path, by extension: the only files xses reads from disk.
fn mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Image part → `Image` block when it fits the inline cap, otherwise a gap.
fn image_block(part: &Value, tool: Tool) -> Block {
    // claude nests the payload one level down:
    // `{type: image, source: {type: base64, media_type, data}}`.
    let part = part
        .get("source")
        .filter(|source| source.is_object())
        .unwrap_or(part);
    let mut mime = part
        .get("mimeType")
        .or_else(|| part.get("mime_type"))
        .or_else(|| part.get("media_type"))
        // opencode `file` parts name it `mime`
        .or_else(|| part.get("mime"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    // pi/omp: {type:image, data, mimeType}; codex: {type:input_image, image_url:"data:…;base64,…"}.
    // `data` is base64, but it can also be a reference (`blob:sha256:<hash>`,
    // `file:`/path) — those blobs live in the agent's own store and are read
    // back so the image transfers for real.
    let raw = part.get("data").and_then(|d| d.as_str());
    let (data, path, bytes) = match raw {
        Some(r) if r.starts_with("blob:") => match resolve_blob(tool, r) {
            Some((b64, n)) => (Some(b64), None, n),
            None => (None, Some(PathBuf::from(r)), 0),
        },
        Some(r) if r.starts_with("file:") || r.starts_with('/') || r.contains("://") => {
            (None, Some(PathBuf::from(r)), 0)
        }
        Some(r) => {
            let bytes = r.len() as u64 * 3 / 4;
            (Some(r.to_string()), None, bytes)
        }
        // codex says `image_url`; opencode's `file` parts say `url`.
        None => match part
            .get("image_url")
            .or_else(|| part.get("url"))
            .and_then(|u| u.as_str())
        {
            Some(url) => match url.split_once("base64,") {
                Some((head, b64)) => {
                    if mime.is_none() {
                        mime = head
                            .trim_start_matches("data:")
                            .split(';')
                            .next()
                            .filter(|m| !m.is_empty())
                            .map(str::to_string);
                    }
                    (Some(b64.to_string()), None, b64.len() as u64 * 3 / 4)
                }
                None => (None, Some(PathBuf::from(url)), 0),
            },
            None => (None, None, 0),
        },
    };
    if bytes > MAX_INLINE_BYTES {
        return Gap {
            kind: "image".into(),
            detail: format!(
                "image ({:.1} MB{}) exceeds the {} MB inline cap",
                bytes as f64 / 1e6,
                mime.as_deref().map(|m| format!(" {m}")).unwrap_or_default(),
                MAX_INLINE_BYTES / 1024 / 1024
            ),
        };
    }
    let path = path.or_else(|| part.get("path").and_then(|p| p.as_str()).map(PathBuf::from));
    Image {
        mime,
        data,
        path,
        bytes,
    }
}

/// Resolve a content-addressed reference (`blob:sha256:<hash>`) against the
/// agent's own blob store, returning base64 of the bytes.
fn resolve_blob(tool: Tool, reference: &str) -> Option<(String, u64)> {
    let hash = reference.rsplit(':').next()?;
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let home = dirs::home_dir()?;
    // Content-addressed: the hash identifies the bytes, and the store that
    // holds them can belong to the sibling agent (pi sessions referencing omp
    // blobs, and vice versa), so both are searched.
    let pi = home.join(".pi/agent/blobs");
    let omp = home.join(".omp/agent/blobs");
    let dirs = match tool {
        Tool::Pi => vec![pi, omp],
        _ => vec![omp, pi],
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(hash) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_INLINE_BYTES {
                return None;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            return Some((base64_encode(&bytes), bytes.len() as u64));
        }
    }
    None
}

/// Minimal base64 encoder (no dependency for one call site).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A tool call whose result never landed in the source store.
fn tool_output_gap(name: &str) -> Block {
    Gap {
        kind: "tool-output".into(),
        detail: format!("{name} ran but its output was not recorded"),
    }
}

fn classify_text(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    if s.starts_with('<') && s.trim_end().ends_with('>') {
        Some(format!("[injected context]\n{s}"))
    } else {
        Some(s.to_string())
    }
}

/// Split a format-native content field into IR blocks.
///
/// `tool` is the source agent; unknown part types retire as gaps naming it so
/// a resumed session shows what its new home could not carry over.
fn collect_parts(content: &Value, role: Role, tool: Tool, out: &mut Events) {
    match content {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(Text {
                    role,
                    text: s.clone(),
                });
            }
        }
        Value::Array(items) => {
            for it in items {
                match it.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        if let Some(t) = classify_text(it.get("text")) {
                            out.push(Text { role, text: t });
                        }
                    }
                    // claude/pi reasoning. Sources often keep only a signature
                    // (claude redacts the text); that is a gap, not silence.
                    Some("thinking") => {
                        let text = it
                            .get("thinking")
                            .or_else(|| it.get("text"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let signature = it
                            .get("signature")
                            .or_else(|| it.get("thinkingSignature"))
                            .and_then(|s| s.as_str())
                            .map(str::to_string);
                        if !text.trim().is_empty() {
                            out.push(Thinking {
                                text: text.to_string(),
                                signature,
                            });
                        } else if signature.is_some() {
                            out.push(Gap {
                                kind: "thinking".into(),
                                detail: "redacted reasoning not carried over".into(),
                            });
                        }
                    }
                    Some("tool_use") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it.get("input").map(|i| i.to_string()).unwrap_or_default(),
                            call_id: it.get("id").and_then(|i| i.as_str()).map(str::to_string),
                            status: None,
                        });
                    }
                    // pi/omp assistant turns pair `toolCall` items with
                    // `toolResult`-role messages (named via toolName).
                    Some("toolCall") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it
                                .get("arguments")
                                .map(|i| i.to_string())
                                .unwrap_or_default(),
                            call_id: it.get("id").and_then(|i| i.as_str()).map(str::to_string),
                            status: None,
                        });
                    }
                    // claude tool results: string or [text|image|tool_reference]
                    Some("tool_result") => {
                        let content = it.get("content").unwrap_or(&Value::Null);
                        let call_id = it
                            .get("tool_use_id")
                            .and_then(|c| c.as_str())
                            .map(str::to_string);
                        let is_error = it
                            .get("is_error")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false);
                        if let Some(items) = content.as_array() {
                            for inner in items {
                                if matches!(
                                    inner.get("type").and_then(|t| t.as_str()),
                                    Some("image" | "input_image")
                                ) {
                                    out.push(image_block(inner, tool));
                                }
                            }
                        }
                        let output = text_of(content).unwrap_or_default();
                        if output.trim().is_empty() {
                            if !content_has_image(content) {
                                out.push(tool_output_gap("tool"));
                            }
                        } else {
                            out.push(ToolResult {
                                name: "tool".into(),
                                output,
                                call_id,
                                is_error,
                                duration_ms: None,
                            });
                        }
                    }
                    Some("function_call") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("function")
                                .to_string(),
                            args: it
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("")
                                .to_string(),
                            call_id: it
                                .get("call_id")
                                .or_else(|| it.get("id"))
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            status: it
                                .get("status")
                                .and_then(|s| s.as_str())
                                .map(str::to_string),
                        });
                    }
                    Some("image" | "input_image") => out.push(image_block(it, tool)),
                    None => {
                        if let Some(s) = it.as_str() {
                            out.push(Text {
                                role,
                                text: s.to_string(),
                            });
                        }
                    }
                    Some(other) => out.push(Gap {
                        kind: format!("{}-part", tool.as_str()),
                        detail: format!("{other} part not carried over"),
                    }),
                }
            }
        }
        _ => {}
    }
}

/// Format-native content (string or typed-parts array) → plain text, for
/// first-message scanning and tool-output capture.
pub fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                match it.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") | Some("output_text") | None => {
                        if let Some(t) = it.get("text").and_then(|t| t.as_str()) {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(t);
                        } else if let Some(s) = it.as_str() {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(s);
                        }
                    }
                    Some("tool_use" | "toolCall" | "function_call") => {
                        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        out.push_str(&format!("\n[previous tool call: {name}]"));
                    }
                    Some("image" | "input_image") => {
                        out.push_str("\n[image attachment]");
                    }
                    Some("tool_reference") => {
                        let name = it
                            .get("tool_name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool");
                        out.push_str(&format!("\n[tool reference: {name}]"));
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::Event;
    use serde_json::json;
    use std::path::Path;

    fn parts(items: Value) -> Vec<Event> {
        let mut out = Events::default();
        collect_parts(&items, Role::Assistant, Tool::Claude, &mut out);
        out.into_vec()
    }

    #[test]
    fn thinking_keeps_text_and_signature() {
        let events = parts(json!([{"type": "thinking", "thinking": "why", "signature": "sig"}]));
        match &events[0].block {
            Block::Thinking { text, signature } => {
                assert_eq!(text, "why");
                assert_eq!(signature.as_deref(), Some("sig"));
            }
            other => panic!("want Thinking, got {other:?}"),
        }
    }

    #[test]
    fn redacted_thinking_retires_as_gap() {
        let events = parts(json!([{"type": "thinking", "thinking": "", "signature": "abc"}]));
        assert!(matches!(&events[0].block, Block::Gap { .. }));
    }

    #[test]
    fn tool_pairs_keep_call_ids() {
        let events = parts(json!([
            {"type": "tool_use", "id": "tu1", "name": "bash", "input": {"command": "ls"}},
            {"type": "tool_result", "tool_use_id": "tu1", "content": "a.txt", "is_error": false},
        ]));
        match (&events[0].block, &events[1].block) {
            (
                Block::ToolCall { call_id: c, .. },
                Block::ToolResult {
                    call_id: r, output, ..
                },
            ) => {
                assert_eq!(c.as_deref(), Some("tu1"));
                assert_eq!(r.as_deref(), Some("tu1"));
                assert_eq!(output, "a.txt");
            }
            other => panic!("want call/result pair, got {other:?}"),
        }
    }

    #[test]
    fn oversized_images_become_gaps_with_reason() {
        let big = "A".repeat(8 * 1024 * 1024);
        let events = parts(json!([{"type": "image", "mimeType": "image/png", "data": big}]));
        match &events[0].block {
            Block::Gap { kind, detail } => {
                assert_eq!(kind, "image");
                assert!(detail.contains("inline cap"), "detail: {detail}");
            }
            other => panic!("want image gap, got {other:?}"),
        }
    }

    #[test]
    fn small_images_carry_base64() {
        let events = parts(json!([{"type": "image", "mimeType": "image/png", "data": "qq=="}]));
        match &events[0].block {
            Block::Image {
                mime, data, bytes, ..
            } => {
                assert_eq!(mime.as_deref(), Some("image/png"));
                assert_eq!(data.as_deref(), Some("qq=="));
                assert_eq!(*bytes, 3u64);
            }
            other => panic!("want Image, got {other:?}"),
        }
    }

    #[test]
    fn claude_images_keep_bytes_and_media_type() {
        // claude nests the payload: {type:image, source:{type:base64,
        // media_type, data}} — the source object carries both fields.
        let events = parts(json!([{
            "type": "image",
            "source": {"type": "base64", "media_type": "image/jpeg", "data": "qq=="}
        }]));
        match &events[0].block {
            Block::Image {
                mime, data, bytes, ..
            } => {
                assert_eq!(
                    mime.as_deref(),
                    Some("image/jpeg"),
                    "claude's media_type must survive: labelling jpeg as png breaks the image"
                );
                assert_eq!(
                    data.as_deref(),
                    Some("qq=="),
                    "claude's base64 must be read"
                );
                assert_eq!(*bytes, 3u64);
            }
            other => panic!("want Image, got {other:?}"),
        }
    }

    #[test]
    fn opencode_file_parts_with_images_carry_bytes() {
        // Shape read from a live store: {"type":"file","mime":"image/webp",
        // "filename":"image-84994.webp","url":"data:image/webp;base64,…"}.
        let mut out = Events::default();
        opencode_part(
            &json!({
                "type": "file",
                "mime": "image/webp",
                "filename": "image-84994.webp",
                "url": "data:image/webp;base64,qq==",
            }),
            Role::User,
            &mut out,
        );
        match &out.into_vec()[0].block {
            Block::Image {
                mime, data, bytes, ..
            } => {
                assert_eq!(mime.as_deref(), Some("image/webp"));
                assert_eq!(data.as_deref(), Some("qq=="));
                assert_eq!(*bytes, 3u64);
            }
            other => panic!("want Image, got {other:?}"),
        }
    }

    /// Write a claude transcript and parse it through the real reader. Each
    /// call gets its own file: tests run in parallel in one process.
    fn claude_rows(rows: &[Value]) -> Vec<Event> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("xses-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "probe-{}.jsonl",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let body: String = rows
            .iter()
            .map(|r| format!("{r}\n"))
            .collect::<Vec<_>>()
            .join("");
        std::fs::write(&path, body).unwrap();
        let events = claude(&path).expect("parse").into_vec();
        let _ = std::fs::remove_file(&path);
        events
    }

    #[test]
    fn claude_attachment_text_is_carried() {
        // Real shape: the file's text is embedded under `content.file`.
        let row = json!({
            "type": "attachment",
            "attachment": {
                "type": "file",
                "filename": "/home/some/README.md",
                "displayPath": "../README.md",
                "content": {
                    "type": "text",
                    "file": {
                        "filePath": "/home/some/README.md",
                        "content": "# Workbook\nline two",
                        "startLine": 1,
                        "totalLines": 83
                    }
                }
            }
        });
        match &claude_rows(&[row])[0].block {
            Block::Context { kind, text, path } => {
                assert_eq!(kind, "attachment");
                let text = text.as_deref().unwrap_or("");
                assert!(
                    text.contains("# Workbook"),
                    "file text must survive: {text}"
                );
                assert!(text.contains("README.md"), "the name identifies it: {text}");
                assert!(text.contains("83 lines"), "line count kept: {text}");
                assert_eq!(path.as_deref(), path.as_deref().filter(|_| true));
            }
            other => panic!("want Context, got {other:?}"),
        }
    }

    #[test]
    fn source_message_times_are_read() {
        let rows = [
            json!({"type": "user", "timestamp": "2026-10-13T09:30:00.500Z",
                   "message": {"role": "user", "content": "hi"}}),
            json!({"type": "assistant", "timestamp": "2026-10-13T09:31:12.250Z",
                   "message": {"role": "assistant", "content": "there"}}),
        ];
        let dir = std::env::temp_dir().join(format!("xses-times-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("times.jsonl");
        let body: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let (events, times) = claude(&path).expect("parse").into_parts();
        let _ = std::fs::remove_file(&path);
        assert_eq!(events.len(), 2);
        let stamps: Vec<String> = times
            .values()
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .collect();
        assert_eq!(
            stamps,
            vec!["2026-10-13T09:30:00.500Z", "2026-10-13T09:31:12.250Z"],
            "each message keeps the time the source recorded"
        );
    }

    #[test]
    fn meta_injections_transfer_once() {
        let skill = json!({
            "type": "user",
            "isMeta": true,
            "message": {"role": "user", "content": [{"type": "text", "text": "# Omarchy Skill\nhow to use it"}]}
        });
        let turn = json!({
            "type": "user",
            "message": {"role": "user", "content": "do the thing"}
        });
        // same injection twice, then a real turn
        let events = claude_rows(&[skill.clone(), skill.clone(), turn.clone()]);
        let contexts: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.block {
                Block::Context { kind, text, .. } if kind == "meta" => text.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(contexts.len(), 1, "one copy of a repeated injection");
        assert!(contexts[0].contains("Omarchy Skill"), "{:?}", contexts[0]);
        assert!(
            !claude_rows(&[turn]).iter().any(|e| matches!(
                &e.block,
                Block::Context { kind, .. } if kind == "meta"
            )),
            "ordinary turns stay ordinary"
        );
    }

    #[test]
    fn compact_reference_keeps_embedded_content_and_inlines_images() {
        // No payload: the reference note, naming the file.
        let row = json!({
            "type": "attachment",
            "attachment": {
                "type": "compact_file_reference",
                "filename": "/home/some/notes.md",
                "displayPath": "../notes.md"
            }
        });
        match &claude_rows(&[row])[0].block {
            Block::Context { kind, text, .. } => {
                assert_eq!(kind, "pre-compaction-reference");
                assert!(
                    text.as_deref().unwrap_or("").contains("re-read"),
                    "{text:?}"
                );
            }
            other => panic!("want Context, got {other:?}"),
        }

        // With embedded text: carry the text, not just the reference.
        let row = json!({
            "type": "attachment",
            "attachment": {
                "type": "compact_file_reference",
                "filename": "/home/some/notes.md",
                "content": {"type": "text", "file": {"content": "notes body", "totalLines": 4}}
            }
        });
        match &claude_rows(&[row])[0].block {
            Block::Context { text, .. } => {
                let text = text.as_deref().unwrap_or("");
                assert!(text.contains("notes body"), "embedded text wins: {text}");
            }
            other => panic!("want Context, got {other:?}"),
        }
    }

    #[test]
    fn claude_attachment_image_on_disk_is_inlined() {
        let dir = std::env::temp_dir().join(format!("xses-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.png");
        std::fs::write(&png, [0x89u8, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();
        let row = json!({
            "type": "attachment",
            "attachment": {"type": "file", "filename": png.to_str().unwrap()}
        });
        match &claude_rows(&[row])[0].block {
            Block::Image {
                mime, data, bytes, ..
            } => {
                assert_eq!(mime.as_deref(), Some("image/png"));
                assert!(data.is_some(), "bytes must be read");
                assert_eq!(*bytes, 8);
            }
            other => panic!("want Image, got {other:?}"),
        }
        // A path that no longer exists says so instead of pretending.
        let row = json!({
            "type": "attachment",
            "attachment": {"type": "file", "filename": "/nonexistent/gone.png"}
        });
        match &claude_rows(&[row])[0].block {
            Block::Context { text, .. } => {
                let text = text.as_deref().unwrap_or("");
                assert!(text.contains("gone"), "{text}");
                assert!(text.contains("gone.png"), "{text}");
            }
            other => panic!("want Context, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_compaction_summary_is_marked() {
        let row = json!({
            "type": "user",
            "isCompactSummary": true,
            "message": {"role": "user", "content": "This session is being continued. Summary: did things."}
        });
        match &claude_rows(&[row])[0].block {
            Block::Compressed { text } => {
                assert!(text.contains("compacted"), "{text}");
                assert!(
                    text.contains("Summary: did things"),
                    "summary text kept: {text}"
                );
            }
            other => panic!("want Compressed, got {other:?}"),
        }
    }

    #[test]
    fn codex_compaction_is_carried_not_dropped() {
        let payload = json!({
            "message": "",
            "replacement_history": [{"type": "message"}, {"type": "message"}],
        });
        match codex_compaction(&payload) {
            Block::Compressed { text } => {
                assert!(text.contains("compacted"), "{text}");
                assert!(text.contains('2'), "retained size gives the scale: {text}");
            }
            other => panic!("want Compressed, got {other:?}"),
        }
        let with_message = json!({"message": "earlier work summarized"});
        match codex_compaction(&with_message) {
            Block::Compressed { text } => assert_eq!(text, "earlier work summarized"),
            other => panic!("want Compressed, got {other:?}"),
        }
    }

    #[test]
    fn empty_tool_output_retires_as_gap() {
        let events = parts(json!([{"type": "tool_result", "content": "  "}]));
        assert!(matches!(&events[0].block, Block::Gap { .. }));
    }

    #[test]
    fn unknown_part_types_retire_as_named_gaps() {
        let mut out = Events::default();
        collect_parts(
            &json!([{"type": "mystery_widget"}]),
            Role::Assistant,
            Tool::Codex,
            &mut out,
        );
        let events = out.into_vec();
        match &events[0].block {
            Block::Gap { kind, .. } => assert_eq!(kind, "codex-part"),
            other => panic!("want gap, got {other:?}"),
        }
    }

    #[test]
    fn file_attachments_become_context_while_reminders_stay_silent() {
        let file = claude_attachment(Some(&json!({"type": "file", "filename": "/tmp/plan.md"})));
        assert!(matches!(file, Some(Block::Context { .. })));
        assert!(claude_attachment(Some(&json!({"type": "total_tokens_reminder"}))).is_none());
        assert!(claude_attachment(Some(&json!({"type": "environment"}))).is_none());
    }

    #[test]
    fn text_of_reads_tool_references() {
        let v = json!([{"type": "tool_reference", "tool_name": "read"}]);
        assert!(text_of(&v).unwrap().contains("read"));
        let _ = Path::new("/tmp");
    }
}
