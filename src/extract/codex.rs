//! codex extractor: rollout JSONL.
use super::common::{any_time, codex_compaction, collect_parts, lines, num, parse_record};
use super::text_of;
use crate::canonical::{Block::*, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub(super) fn codex(path: &Path) -> Result<Events> {
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
                        if let Some(ts) = any_time(&v) {
                            out.at(ts);
                        }
                        if let Some(c) = payload.get("content") {
                            collect_parts(c, role, Tool::Codex, &mut out);
                        }
                    }
                    Some("reasoning") => {
                        out.boundary();
                        if let Some(ts) = any_time(&v) {
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
                        if let Some(ts) = any_time(&v) {
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
                        if let Some(ts) = any_time(&v) {
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
                if let Some(ts) = any_time(&v) {
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
                                provider: None,
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
