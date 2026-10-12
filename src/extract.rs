//! Extract: source transcript → IR (`CanonicalSession`).
//!
//! One extractor per source tool. Sources are strictly read-only.
use crate::canonical::{Block, CanonicalSession, Role};
use crate::model::Tool;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub fn extract(
    tool: Tool,
    id: &str,
    cwd: &Path,
    created: chrono::DateTime<chrono::Utc>,
    path: &Path,
) -> Result<CanonicalSession> {
    let blocks = match tool {
        Tool::Claude => claude(path)?,
        Tool::Codex => codex(path)?,
        Tool::Pi | Tool::Omp => pi_family(path)?,
        Tool::Opencode => opencode(id)?,
    };
    Ok(CanonicalSession {
        id: id.to_string(),
        tool,
        cwd: cwd.to_path_buf(),
        created,
        blocks,
    })
}

/// opencode transcripts via read-only SQL (message + part `data` JSON).
/// The runtime shapes are identical to `opencode export` output, but SQL is
/// deterministic: the CLI truncates large stdout to 64KB multiples with exit 0.
fn opencode(id: &str) -> Result<Vec<Block>> {
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
        let text = String::from_utf8_lossy(&q.stdout);
        Ok(serde_json::from_str(text.trim()).unwrap_or_default())
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
    let mut blocks = Vec::new();
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
        if let Some(items) = by_msg.get(mid) {
            for part in items {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = classify_text(part.get("text")) {
                            blocks.push(Block::Text { role, text: t });
                        }
                    }
                    Some("reasoning") => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            blocks.push(Block::Thinking {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("tool") => {
                        let name = part
                            .get("tool")
                            .and_then(|t| t.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let input = part
                            .pointer("/state/input")
                            .map(|i| i.to_string())
                            .unwrap_or_default();
                        let output = part
                            .pointer("/state/output")
                            .map(|o| o.to_string())
                            .unwrap_or_default();
                        blocks.push(Block::ToolCall {
                            name: name.clone(),
                            args: input,
                        });
                        if !output.is_empty() {
                            blocks.push(Block::ToolResult { name, output });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(blocks)
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    Ok(BufReader::new(fs::File::open(path)?).lines())
}

fn parse_line(path: &Path, line_no: usize, line: &str) -> Result<Value> {
    serde_json::from_str(line)
        .with_context(|| format!("parse {} line {}", path.display(), line_no + 1))
}

fn claude(path: &Path) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v = parse_line(path, line_no, &line)?;
        let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if t != "user" && t != "assistant" && t != "system" {
            // sidechains/todos/etc never carried
            continue;
        }
        if v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false)
            || v.get("isSidechain")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
        {
            continue;
        }
        let role = match t {
            "user" => Role::User,
            "system" => Role::System,
            _ => Role::Assistant,
        };
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        collect_parts(content, role, &mut out);
    }
    Ok(out)
}

fn codex(path: &Path) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v = parse_line(path, line_no, &line)?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response_item") => match v.pointer("/payload/type").and_then(|t| t.as_str()) {
                Some("message") => {
                    let role = match v.pointer("/payload/role").and_then(|r| r.as_str()) {
                        Some("user") => Role::User,
                        Some("system") => Role::System,
                        _ => Role::Assistant,
                    };
                    if let Some(c) = v.pointer("/payload/content") {
                        collect_parts(c, role, &mut out);
                    }
                }
                Some("reasoning") => {
                    if let Some(t) = v.pointer("/payload/content").and_then(|c| c.as_str()) {
                        out.push(Block::Thinking {
                            text: t.to_string(),
                        });
                    }
                }
                Some("function_call") => {
                    out.push(Block::ToolCall {
                        name: v
                            .pointer("/payload/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string(),
                        args: v
                            .pointer("/payload/arguments")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
                Some("function_call_output") => {
                    let o = v
                        .pointer("/payload/output")
                        .map(|o| o.to_string())
                        .unwrap_or_default();
                    out.push(Block::ToolResult {
                        name: "function".into(),
                        output: o,
                    });
                }
                _ => {}
            },
            Some("event_msg") => {
                if let Some(t) = v.pointer("/payload/text").and_then(|t| t.as_str()) {
                    if !t.trim().is_empty() {
                        out.push(Block::Text {
                            role: Role::System,
                            text: t.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn pi_family(path: &Path) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v = parse_line(path, line_no, &line)?;
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let role = match v.pointer("/message/role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("system") => Role::System,
            _ => Role::Assistant,
        };
        if let Some(c) = v.pointer("/message/content") {
            collect_parts(c, role, &mut out);
        }
    }
    Ok(out)
}

/// Split a format-native content field into IR blocks.
fn collect_parts(content: &Value, role: Role, out: &mut Vec<Block>) {
    match content {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(Block::Text {
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
                            out.push(Block::Text { role, text: t });
                        }
                    }
                    Some("tool_use") => {
                        out.push(Block::ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it.get("input").map(|i| i.to_string()).unwrap_or_default(),
                        });
                    }
                    Some("tool_result") => {
                        out.push(Block::ToolResult {
                            name: "tool".into(),
                            output: text_of(it.get("content").unwrap_or(&Value::Null))
                                .unwrap_or_default(),
                        });
                    }
                    Some("function_call") => {
                        out.push(Block::ToolCall {
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
                        });
                    }
                    None => {
                        if let Some(s) = it.as_str() {
                            out.push(Block::Text {
                                role,
                                text: s.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
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

/// Format-native content (string or typed-parts array) → plain text, for
/// first-message scanning. The IR's typed path is `collect_parts`.
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
                        }
                    }
                    Some("tool_use") => {
                        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        out.push_str(&format!("\n[tool call: {name}]"));
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
