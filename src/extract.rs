//! Extract: source transcript → IR (`CanonicalSession`).
//!
//! One extractor per source tool. Sources are strictly read-only.
use crate::canonical::{Block, CanonicalSession, Role};
use crate::model::Tool;
use anyhow::Result;
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
        Ok(crate::proc::json_rows(&text, "opencode transcript").unwrap_or_default())
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
                        blocks.push(Block::ToolCall {
                            name: name.clone(),
                            args: input,
                        });
                        match part.pointer("/state/output") {
                            Some(o) => {
                                let output = o.to_string();
                                if output.is_empty() {
                                    blocks.push(tool_output_gap(&name));
                                } else {
                                    blocks.push(Block::ToolResult { name, output });
                                }
                            }
                            // 181/11148 live tool parts carry no output key: the
                            // call ran but its result never landed in the DB.
                            None => blocks.push(tool_output_gap(&name)),
                        }
                    }
                    Some("compaction") => blocks.push(Block::Gap {
                        kind: "compaction".into(),
                        detail: "session was compacted; earlier history summarized away".into(),
                    }),
                    Some("patch") => {
                        let files = part
                            .get("files")
                            .and_then(|f| f.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0);
                        blocks.push(Block::Gap {
                            kind: "patch".into(),
                            detail: format!(
                                "code patch ({} file{}) not carried over",
                                files,
                                if files == 1 { "" } else { "s" }
                            ),
                        });
                    }
                    // step-start/step-finish are run bookkeeping, file parts
                    // duplicate text, subtask/agent parts are orchestration:
                    // none carry user-visible transcript content.
                    Some("step-start" | "step-finish" | "file" | "subtask" | "agent") => {}
                    Some(other) => blocks.push(Block::Gap {
                        kind: "opencode-part".into(),
                        detail: format!("opencode {other} part not carried over"),
                    }),
                    None => {}
                }
            }
        }
    }
    Ok(blocks)
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    Ok(BufReader::new(fs::File::open(path)?).lines())
}

fn claude(path: &Path) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = crate::jsonl::parse_record(line_no, &line, &mut out) else {
            continue;
        };
        let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false)
            || v.get("isSidechain")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
        {
            continue;
        }
        match t {
            "user" | "assistant" | "system" => {
                let role = match t {
                    "user" => Role::User,
                    "system" => Role::System,
                    _ => Role::Assistant,
                };
                let Some(content) = v.pointer("/message/content") else {
                    continue;
                };
                collect_parts(content, role, Tool::Claude, &mut out);
            }
            // Attachments are out-of-band context, not transcript turns: only
            // content-bearing ones (user files, compaction pointers) retire as
            // gaps. Reminders/snapshots/listings stay silent.
            "attachment" => {
                if let Some(gap) = claude_attachment_gap(v.get("attachment")) {
                    out.push(gap);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Content-bearing claude attachment → gap; environment noise → silent.
fn claude_attachment_gap(a: Option<&Value>) -> Option<Block> {
    let a = a?;
    match a.get("type").and_then(|t| t.as_str()) {
        Some("file") => {
            let name = a
                .get("filename")
                .and_then(|f| f.as_str())
                .and_then(|f| f.rsplit('/').next())
                .unwrap_or("file");
            Some(Block::Gap {
                kind: "attachment".into(),
                detail: format!("attached file {name} not carried over"),
            })
        }
        // Pre-compaction file pointer: the content it names is already gone
        // from the source; the resumed session only needs the reference.
        Some("compact_file_reference") => {
            let name = a
                .get("displayPath")
                .and_then(|d| d.as_str())
                .unwrap_or("file");
            Some(Block::Gap {
                kind: "attachment".into(),
                detail: format!("pre-compaction reference to {name}; re-read if needed"),
            })
        }
        Some("thinking_stripped") => Some(Block::Gap {
            kind: "thinking".into(),
            detail: "reasoning stripped before import; not carried over".into(),
        }),
        Some("queued_command") => {
            let status = a
                .get("prompt")
                .and_then(|p| p.as_str())
                .and_then(|p| p.split("<status>").nth(1)?.split("</status>").next())
                .unwrap_or("event");
            Some(Block::Gap {
                kind: "background-task".into(),
                detail: format!("background task {status}; output not carried over"),
            })
        }
        _ => None,
    }
}

fn codex(path: &Path) -> Result<Vec<Block>> {
    let mut out = Vec::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = crate::jsonl::parse_record(line_no, &line, &mut out) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("response_item") => match v.pointer("/payload/type").and_then(|t| t.as_str()) {
                Some("message") => {
                    let role = match v.pointer("/payload/role").and_then(|r| r.as_str()) {
                        Some("user") => Role::User,
                        Some("system") => Role::System,
                        _ => Role::Assistant,
                    };
                    if let Some(c) = v.pointer("/payload/content") {
                        collect_parts(c, role, Tool::Codex, &mut out);
                    }
                }
                Some("reasoning") => {
                    match v.pointer("/payload/content").and_then(|c| c.as_str()) {
                        Some(t) if !t.trim().is_empty() => {
                            out.push(Block::Thinking {
                                text: t.to_string(),
                            });
                        }
                        // codex writes reasoning placeholders with null content
                        // (89/89 live); the shape carries no transcript value.
                        _ => {}
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
                    push_tool_result(&mut out, "function".into(), o);
                }
                Some("custom_tool_call") => {
                    let payload = v.pointer("/payload").unwrap_or(&Value::Null);
                    out.push(Block::ToolCall {
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
                    });
                }
                Some("custom_tool_call_output") => {
                    let output = v
                        .pointer("/payload/output")
                        .and_then(text_of)
                        .unwrap_or_default();
                    push_tool_result(&mut out, "exec".into(), output);
                }
                Some(other) => out.push(Block::Gap {
                    kind: "codex-part".into(),
                    detail: format!("codex {other} part not carried over"),
                }),
                None => {}
            },
            // event_msg item/task lifecycle mirrors response_items; only
            // free-text payloads (turn_aborted, notices) carry transcript.
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
        let Some(v) = crate::jsonl::parse_record(line_no, &line, &mut out) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let msg_role = v.pointer("/message/role").and_then(|r| r.as_str());
        let Some(c) = v.pointer("/message/content") else {
            continue;
        };
        match msg_role {
            Some("toolResult") => {
                // pi/omp name results via the message, not the parts: every
                // toolResult message (9521 text + 39 image live) names its
                // tool in toolName.
                let name = v
                    .pointer("/message/toolName")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let output = text_of(c).unwrap_or_default();
                if image_part_present(c) {
                    out.push(Block::Gap {
                        kind: "image".into(),
                        detail: format!("{name} returned an image; not carried over"),
                    });
                }
                push_tool_result(&mut out, name, output);
            }
            _ => {
                let role = match msg_role {
                    Some("user") => Role::User,
                    Some("system") | Some("developer") => Role::System,
                    _ => Role::Assistant,
                };
                // pi and omp share the v3 schema; omp is the target-local
                // variant, so unknown parts name the family generically.
                collect_parts(c, role, Tool::Pi, &mut out);
            }
        }
    }
    Ok(out)
}

fn image_part_present(content: &Value) -> bool {
    content.as_array().is_some_and(|items| {
        items.iter().any(|it| {
            matches!(
                it.get("type").and_then(|t| t.as_str()),
                Some("image" | "input_image")
            )
        })
    })
}

/// True when a result content carries real text (not marker placeholders).
fn content_has_text(content: &Value) -> bool {
    match content {
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(items) => {
            items
                .iter()
                .any(|it| match it.get("type").and_then(|t| t.as_str()) {
                    Some("text" | "input_text" | "output_text") => it
                        .get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| !t.trim().is_empty()),
                    None => it.as_str().is_some_and(|s| !s.trim().is_empty()),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// Split a format-native content field into IR blocks.
///
/// `tool` is the source agent; unknown part types retire as gaps naming it so
/// a resumed session shows what its new home could not carry over.
fn collect_parts(content: &Value, role: Role, tool: crate::model::Tool, out: &mut Vec<Block>) {
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
                    Some("thinking") => {
                        match it.get("thinking").and_then(|t| t.as_str()) {
                            Some(t) if !t.trim().is_empty() => {
                                out.push(Block::Thinking {
                                    text: t.to_string(),
                                });
                            }
                            // claude redacts thinking to `{thinking: "", signature}`
                            // (1471/1476 live blocks); pi keeps empty signed
                            // placeholders. Both mean reasoning existed but its
                            // content is gone.
                            _ if it.get("signature").is_some()
                                || it.get("thinkingSignature").is_some() =>
                            {
                                out.push(Block::Gap {
                                    kind: "thinking".into(),
                                    detail: "redacted reasoning not carried over".into(),
                                });
                            }
                            _ => {}
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
                    // pi/omp assistant turns pair `toolCall` items with
                    // `toolResult`-role messages (named via toolName).
                    Some("toolCall") => {
                        out.push(Block::ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it
                                .get("arguments")
                                .map(|i| i.to_string())
                                .unwrap_or_default(),
                        });
                    }
                    Some("tool_result") => {
                        let content = it.get("content").unwrap_or(&Value::Null);
                        let has_image = image_part_present(content);
                        if has_image && !content_has_text(content) {
                            // Image-only results (29/33 live): one gap names the
                            // loss; no separate tool-output gap stacks behind it.
                            out.push(Block::Gap {
                                kind: "image".into(),
                                detail: "image-only tool result not carried over".into(),
                            });
                        } else {
                            // Mixed results: keep the text, mark the lost pixels.
                            if has_image {
                                out.push(Block::Gap {
                                    kind: "image".into(),
                                    detail: "image in tool result not carried over".into(),
                                });
                            }
                            push_tool_result(
                                out,
                                "tool".into(),
                                text_of(content).unwrap_or_default(),
                            );
                        }
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
                    Some("image" | "input_image") => {
                        out.push(Block::Gap {
                            kind: "image".into(),
                            detail: "image attachment not carried over".into(),
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
                    Some(other) => out.push(Block::Gap {
                        kind: format!("{}-part", tool.as_str()),
                        detail: format!("{other} part not carried over"),
                    }),
                }
            }
        }
        _ => {}
    }
}

/// A tool call whose result never landed in the source store.
fn tool_output_gap(name: &str) -> Block {
    Block::Gap {
        kind: "tool-output".into(),
        detail: format!("{name} ran but its output was not recorded"),
    }
}

/// Empty-output tool results are indistinguishable from lost ones at resume
/// time, so they retire as explicit gaps instead of silent blanks.
fn push_tool_result(out: &mut Vec<Block>, name: String, output: String) {
    if output.trim().is_empty() {
        out.push(tool_output_gap(&name));
    } else {
        out.push(Block::ToolResult { name, output });
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
                    Some("tool_use" | "toolCall" | "function_call") => {
                        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        out.push_str(&format!("\n[previous tool call: {name}]"));
                    }
                    Some("image" | "input_image") => {
                        out.push_str("\n[image attachment]");
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
    use serde_json::json;

    #[test]
    fn redacted_thinking_retires_as_gap() {
        let mut out = Vec::new();
        collect_parts(
            &json!([{"type": "thinking", "thinking": "", "signature": "abc"}]),
            Role::Assistant,
            Tool::Claude,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Block::Gap { .. }));
    }

    #[test]
    fn pi_toolcall_extracts_with_name_and_args() {
        let mut out = Vec::new();
        collect_parts(
            &json!([{"type": "toolCall", "name": "bash", "arguments": {"command": "ls"}}]),
            Role::Assistant,
            Tool::Pi,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Block::ToolCall { name, args } => {
                assert_eq!(name, "bash");
                assert!(args.contains("ls"));
            }
            other => panic!("want ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn image_only_tool_result_yields_single_image_gap() {
        let mut out = Vec::new();
        collect_parts(
            &json!([{
                "type": "tool_result",
                "content": [{"type": "image", "source": {"data": "qq=="}}],
            }]),
            Role::User,
            Tool::Claude,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Block::Gap { kind, .. } => assert_eq!(kind, "image"),
            other => panic!("want image Gap, got {other:?}"),
        }
    }

    #[test]
    fn empty_tool_output_retires_as_gap() {
        let mut out = Vec::new();
        push_tool_result(&mut out, "bash".into(), "  ".into());
        assert_eq!(out.len(), 1);
        match &out[0] {
            Block::Gap { kind, detail } => {
                assert_eq!(kind, "tool-output");
                assert!(detail.contains("bash"));
            }
            other => panic!("want tool-output Gap, got {other:?}"),
        }
    }

    #[test]
    fn unknown_part_types_retire_as_named_gaps() {
        let mut out = Vec::new();
        collect_parts(
            &json!([{"type": "mystery_widget", "x": 1}]),
            Role::Assistant,
            Tool::Codex,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Block::Gap { kind, detail } => {
                assert_eq!(kind, "codex-part");
                assert!(detail.contains("mystery_widget"));
            }
            other => panic!("want codex-part Gap, got {other:?}"),
        }
    }

    #[test]
    fn file_attachments_retire_as_gaps_while_reminders_stay_silent() {
        let file = claude_attachment_gap(Some(&json!({
            "type": "file",
            "filename": "/tmp/notes/plan.md",
        })));
        assert!(matches!(file, Some(Block::Gap { .. })));
        let reminder = claude_attachment_gap(Some(&json!({"type": "total_tokens_reminder"})));
        assert!(reminder.is_none());
        let env = claude_attachment_gap(Some(&json!({"type": "environment"})));
        assert!(env.is_none());
    }
}
