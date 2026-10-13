//! opencode extractor: read-only SQL over the message/part tables.
use super::common::{any_time, classify_text, image_block, num, tool_output_gap};
use crate::canonical::{Block::*, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;


/// opencode transcripts via read-only SQL (message + part `data` JSON).
/// The runtime shapes are identical to `opencode export` output, but SQL is
/// deterministic: the CLI truncates large stdout to 64KB multiples with exit 0.
/// Dispatches on store generation: v2 (`session_v2`/`session_message`,
/// opencode ≥ 2.x) keeps content inline per message; v1 (`message`+`part`)
/// is kept for older stores and pre-upgrade sessions.
pub(super) fn opencode(id: &str) -> Result<Events> {
    if opencode_is_v2(id) {
        return opencode_v2(id);
    }
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
        crate::proc::json_rows(&String::from_utf8_lossy(&q.stdout), "opencode transcript")
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
        if let Some(ts) = any_time(&m) {
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

/// True when this session lives in the v2 generation. Any SQL failure
/// (older store without the tables) means v1.
fn opencode_is_v2(id: &str) -> bool {
    let Some(db) = crate::scan::opencode_db() else {
        return false;
    };
    let esc = id.replace('\'', "''");
    std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg(&db)
        .arg(format!(
            "SELECT 1 FROM session_v2 WHERE id = '{esc}' LIMIT 1;"
        ))
        .output()
        .map(|q| String::from_utf8_lossy(&q.stdout).contains('1'))
        .unwrap_or(false)
}

/// v2 transcripts: one row per message in `seq` order, content inline.
/// Shapes mirror `opencode session export` (verified live): user rows carry
/// `text` + `files[]`, assistant rows carry `content[]` with inline
/// `text`/`reasoning`/`tool` blocks plus message-level tokens/cost/model.
fn opencode_v2(id: &str) -> Result<Events> {
    let Some(db) = crate::scan::opencode_db() else {
        anyhow::bail!("opencode database not found")
    };
    let esc = id.replace('\'', "''");
    let q = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(&db)
        .arg(format!(
            "SELECT type, data, time_created FROM session_message \
             WHERE session_id = '{esc}' ORDER BY seq;"
        ))
        .output()?;
    if !q.status.success() {
        anyhow::bail!(
            "sqlite read failed: {}",
            String::from_utf8_lossy(&q.stderr).trim()
        );
    }
    let rows = crate::proc::json_rows(&String::from_utf8_lossy(&q.stdout), "opencode transcript")?;
    let mut out = Events::default();
    for row in rows {
        let kind = row.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let Some(data) = row
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        else {
            continue;
        };
        out.boundary();
        if let Some(ms) = row.get("time_created").and_then(|t| t.as_i64()) {
            if let Some(ts) = chrono::DateTime::from_timestamp_millis(ms) {
                out.at(ts.with_timezone(&chrono::Utc));
            }
        }
        match kind {
            "user" => {
                if let Some(t) = classify_text(data.get("text")) {
                    out.push(Text {
                        role: Role::User,
                        text: t,
                    });
                }
                if let Some(files) = data.get("files").and_then(|f| f.as_array()) {
                    for f in files {
                        let mime = f.get("mime").and_then(|m| m.as_str()).unwrap_or("");
                        if mime.starts_with("image/") {
                            out.push(image_block(f, Tool::Opencode));
                        } else if let Some(name) =
                            f.get("name").and_then(|n| n.as_str())
                        {
                            out.push(Context {
                                kind: "file".into(),
                                text: Some(name.to_string()),
                                path: None,
                            });
                        }
                    }
                }
            }
            "assistant" => {
                // Message-level accounting first, so writers see usage.
                let tokens = data.get("tokens").cloned().unwrap_or(Value::Null);
                let (model, provider) = match data.get("model") {
                    Some(m) => (
                        m.get("id").and_then(|v| v.as_str()).map(str::to_string),
                        m.get("providerID").and_then(|v| v.as_str()).map(str::to_string),
                    ),
                    None => (None, None),
                };
                if tokens.is_object() || data.get("cost").is_some() {
                    out.push(Usage {
                        input: num(&tokens, "/input"),
                        output: num(&tokens, "/output"),
                        reasoning: num(&tokens, "/reasoning"),
                        cache_read: num(&tokens, "/cache/read"),
                        cache_write: num(&tokens, "/cache/write"),
                        cost: data.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0),
                        model,
                        provider,
                    });
                }
                if let Some(items) = data.get("content").and_then(|c| c.as_array()) {
                    for block in items {
                        opencode_v2_content(block, &mut out);
                    }
                }
            }
            "system" => {
                if let Some(t) = classify_text(data.get("text")) {
                    out.push(Text {
                        role: Role::System,
                        text: t,
                    });
                }
            }
            "compaction" => {
                let summary =
                    data.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                let recent = data.get("recent").and_then(|s| s.as_str()).unwrap_or("");
                out.push(Compressed {
                    text: if recent.is_empty() {
                        summary.to_string()
                    } else {
                        format!("{summary}\n{recent}")
                    },
                });
            }
            // Run bookkeeping the server manages, not transcript content.
            "idle" | "synthetic" | "agent-switched" | "model-switched" | "location-switched" => {}
            other => out.push(Gap {
                kind: "opencode-message".into(),
                detail: format!("opencode {other} message not carried over"),
            }),
        }
    }
    Ok(out)
}

/// One inline v2 content block → IR blocks.
pub(super) fn opencode_v2_content(block: &Value, out: &mut Events) {
    match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = classify_text(block.get("text")) {
                out.push(Text {
                    role: Role::Assistant,
                    text: t,
                });
            }
        }
        Some("reasoning") => {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    out.push(Thinking {
                        text: t.to_string(),
                        signature: None,
                    });
                }
            }
        }
        Some("tool") => {
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("tool")
                .to_string();
            let call_id = block
                .get("id")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            // v2 calls name the id `id` (v1: `callID`) and the tool `name`
            // (v1: `tool`); states split into completed (`content[]`) vs
            // error (`error.message`) shapes.
            let state = block.get("state").cloned().unwrap_or(Value::Null);
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
                .get("content")
                .and_then(|c| c.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|o| !o.is_empty());
            let error = state
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string);
            let duration = block
                .pointer("/time/completed")
                .and_then(|e| e.as_i64())
                .zip(block.pointer("/time/created").and_then(|s| s.as_i64()))
                .map(|(end, start)| (end - start).max(0) as u64);
            match (output, error) {
                (Some(o), _) => out.push(ToolResult {
                    name: name.clone(),
                    output: o,
                    call_id: call_id.clone(),
                    is_error: status.as_deref() == Some("error"),
                    duration_ms: duration,
                }),
                (_, Some(e)) => out.push(ToolResult {
                    name: name.clone(),
                    output: e,
                    call_id: call_id.clone(),
                    is_error: true,
                    duration_ms: duration,
                }),
                _ => out.push(tool_output_gap(&name)),
            }
        }
        // v2 has no patch/file assistant blocks; images ride on user rows.
        Some(other) => out.push(Gap {
            kind: "opencode-content".into(),
            detail: format!("opencode {other} block not carried over"),
        }),
        None => {}
    }
}
/// One v1 opencode part → blocks. Kept separate from the message walk so the part
/// vocabulary can be exercised directly.
pub(super) fn opencode_part(part: &Value, role: Role, out: &mut Events) {
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
                provider: None,
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

