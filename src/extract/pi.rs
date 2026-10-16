//! pi / oh-my-pi extractor: v3 JSONL sessions.
use super::common::{
    any_time, collect_parts, content_has_image, image_block, lines, num, parse_record,
    tool_output_gap,
};
use super::text_of;
use crate::canonical::{Block::*, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub(super) fn pi_family(path: &Path, tool: Tool) -> Result<Events> {
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
                if msg_role == Some("compactionSummary") {
                    out.boundary();
                    if let Some(ts) = any_time(&v) {
                        out.at(ts);
                    }
                    out.push(Compressed {
                        text: msg
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("session compacted")
                            .to_string(),
                    });
                    continue;
                }
                let Some(content) = msg.get("content") else {
                    continue;
                };
                out.boundary();
                if let Some(ts) = any_time(&v) {
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
                                provider: msg
                                    .get("provider")
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
