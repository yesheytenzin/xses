//! codex [`Roller`]: one transcript block → rollout records, threading id state.
//!
//! Kept separate so `rollout.rs` stays the thin entry point (`rollout_items`
//! + helpers) while the block-by-block mapping lives here.
use super::super::fallback_text;
use super::tools::{classify_tool, ThreadTool};
use super::Item;
use crate::canonical::{Block, Role};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Schema-faithful `commandExecution` projection row (v2 `ThreadItem`).
/// Every field the schema requires without a default is present: the
/// resume loader parses each page strictly, and a missing `commandActions`
/// or `processId` fails the whole page (verified against codex 0.162
/// sources + live native rows). Nullable unknowns stay null rather than
/// invented.
fn command_execution(
    id: &str,
    command: &str,
    cwd: &std::path::Path,
    status: &str,
    output: &str,
    exit_code: Option<i64>,
    duration_ms: Option<u64>,
) -> Value {
    json!({
        "type": "commandExecution",
        "id": id,
        "pluginId": null,
        "scriptPath": null,
        "processId": null,
        "command": command,
        "cwd": cwd,
        "commandActions": [{"type": "unknown", "command": command}],
        "source": "agent",
        "status": status,
        "aggregatedOutput": output,
        "exitCode": exit_code,
        "durationMs": duration_ms,
    })
}

/// Schema-faithful `mcpToolCall` projection row: the schema's required
/// fields plus explicit nulls for every non-defaulted `Option` (a missing
/// key fails just like a wrong type). Statuses are camelCase.
fn mcp_tool_call(
    id: &str,
    server: &str,
    tool: &str,
    status: &str,
    arguments: Value,
    result: Value,
) -> Value {
    json!({
        "type": "mcpToolCall",
        "id": id,
        "server": server,
        "tool": tool,
        "status": status,
        "arguments": arguments,
        "appContext": null,
        "mcpAppResourceUri": null,
        "mcpAppUi": null,
        "pluginId": null,
        "readOnlyHint": null,
        "result": result,
        "error": null,
        "durationMs": null,
    })
}

pub(super) fn is_shellish(name: &str) -> bool {
    matches!(
        name,
        "exec" | "exec_command" | "bash" | "shell" | "sh" | "terminal"
    )
}

/// Turns one transcript block into rollout records, threading id state.
pub(super) struct Roller<'a> {
    pub(super) out: Vec<Item>,
    pub(super) n: usize,
    pub(super) pending: Vec<(Option<String>, String, ThreadTool)>,
    pub(super) ids: super::super::TargetIds,
    pub(super) cwd: &'a std::path::Path,
}

impl Roller<'_> {
    fn mint(&mut self, prefix: &str) -> String {
        self.n += 1;
        format!("{prefix}{:05}", self.n)
    }

    /// Import note: the resumed agent must read the transcript as history.
    pub(super) fn push_import_note(&mut self) {
        let id = self.mint("msg_");
        self.out.push(Item {
            wrapper: "response_item",
            payload: json!({
                "type": "message",
                "id": id,
                "role": "developer",
                "content": [{"type": "input_text", "text": super::super::IMPORT_NOTE}],
            }),
            thread: Some((
                "agentMessage".into(),
                json!({"type": "agentMessage", "id": id, "text": super::super::IMPORT_NOTE}),
            )),
            item_id: id,
            ts: None,
        });
    }

    pub(super) fn push_block(&mut self, block: &Block, ts: Option<chrono::DateTime<chrono::Utc>>) {
        match block {
            Block::Text { role, text, .. } => self.push_text(role, text, ts),
            Block::Thinking { text, .. } => self.push_thinking(text, ts),
            Block::ToolCall {
                name,
                args,
                call_id,
                ..
            } => self.push_tool_call(name, args, call_id, ts),
            Block::ToolResult {
                name,
                output,
                call_id,
                is_error,
                duration_ms,
            } => self.push_tool_result(name, output, call_id, *is_error, *duration_ms, ts),
            Block::Image {
                mime, data, path, ..
            } => self.push_image(mime, data, path, ts),
            Block::Context { .. } => {
                self.push_fallback(block, "ctx_", "input_text", "developer", ts)
            }
            Block::Patch { .. } => {
                self.push_fallback(block, "patch_", "output_text", "assistant", ts)
            }
            Block::Compressed { .. } => self.push_compaction(block, ts),
            Block::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                ..
            } => self.push_usage(*input, *output, *reasoning, *cache_read, *cache_write, ts),
            Block::Gap { .. } => self.push_fallback(block, "gap_", "input_text", "system", ts),
        }
    }

    fn push_text(&mut self, role: &Role, text: &String, ts: Option<chrono::DateTime<chrono::Utc>>) {
        let id = self.mint("msg_");
        let (wrapper_role, text_type) = match role {
            Role::User => ("user", "input_text"),
            Role::System => ("system", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
        };
        self.out.push(Item {
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
            ts,
        });
    }
    fn push_thinking(&mut self, text: &String, ts: Option<chrono::DateTime<chrono::Utc>>) {
        if text.trim().is_empty() {
            return;
        }
        let id = self.mint("rs_");
        self.out.push(Item {
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
            ts,
        });
    }
    fn push_tool_call(
        &mut self,
        name: &String,
        args: &String,
        call_id: &Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let shellish = is_shellish(name);
        let id = call_id
            .clone()
            .map(|c| self.ids.get(&c))
            .unwrap_or_else(|| self.mint("call_"));
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
        self.pending
            .push((Some(id.clone()), name.clone(), tool.clone()));
        let thread = match &tool {
            ThreadTool::Shell(command) => (
                "commandExecution",
                command_execution(&id, command, self.cwd, "inProgress", "", None, None),
            ),
            // codex's TUI draws a patch as a file change, not as a
            // command; the diff lives in the call's arguments.
            ThreadTool::FileChange(changes) => (
                "fileChange",
                json!({
                    "type": "fileChange",
                    "id": id,
                    "changes": changes,
                    "status": "inProgress",
                }),
            ),
            // Intentional fallback: unstructured args land as null
            // rather than failing the import; the rollout still
            // names the tool and server.
            ThreadTool::Mcp { server, tool } => (
                "mcpToolCall",
                mcp_tool_call(
                    &id,
                    server,
                    tool,
                    "inProgress",
                    serde_json::from_str::<Value>(args).unwrap_or(Value::Null),
                    Value::Null,
                ),
            ),
            ThreadTool::ImageView(path) => (
                "imageView",
                json!({"type": "imageView", "id": id, "path": path}),
            ),
        };
        self.out.push(Item {
            wrapper: "response_item",
            payload,
            thread: Some((thread.0.to_string(), thread.1)),
            item_id: id,
            ts,
        });
    }
    fn push_tool_result(
        &mut self,
        name: &String,
        output: &String,
        call_id: &Option<String>,
        is_error: bool,
        duration_ms: Option<u64>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        // Same mapping the call side used, so both sides of the pair share
        // one target-safe id (`call_x|fc_y` sources map to `call_x`) and the
        // pending lookup below compares like with like.
        let id = call_id
            .clone()
            .map(|c| self.ids.get(&c))
            .or_else(|| {
                self.pending
                    .iter()
                    .rev()
                    .find(|(c, n, _)| c.is_none() && n == name)
                    .and_then(|(c, _, _)| c.clone())
            })
            .unwrap_or_else(|| self.mint("call_"));
        let shellish = self
            .pending
            .iter()
            .any(|(c, n, _)| n == name && (c.as_deref() == Some(&id) || c.is_none()));
        let wrapper_payload = if shellish && is_shellish(name) {
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
        let tool = self
            .pending
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
            // A successful patch lands as the completed change itself
            // (same id replaces the in-progress row from the call side).
            ThreadTool::FileChange(changes) if !is_error => Some((
                "fileChange".to_string(),
                json!({
                    "type": "fileChange",
                    "id": id,
                    "changes": changes,
                    "status": "completed",
                }),
            )),
            ThreadTool::FileChange(_) => None,
            ThreadTool::ImageView(_) => None,
            ThreadTool::Mcp { server, tool: mcp } => Some((
                "mcpToolCall".to_string(),
                mcp_tool_call(
                    &id,
                    server,
                    mcp,
                    if is_error { "failed" } else { "completed" },
                    Value::Null,
                    json!({"content": [{"type": "text", "text": output}]}),
                ),
            )),
            _ => Some((
                "commandExecution".to_string(),
                command_execution(
                    &id,
                    &command,
                    self.cwd,
                    if is_error { "failed" } else { "completed" },
                    output,
                    None,
                    duration_ms,
                ),
            )),
        };
        self.out.push(Item {
            wrapper: "response_item",
            payload: wrapper_payload,
            thread,
            item_id: id,
            ts,
        });
    }
    fn push_image(
        &mut self,
        mime: &Option<String>,
        data: &Option<String>,
        path: &Option<PathBuf>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let id = self.mint("img_");
        let payload = match data {
            Some(d) => json!({
                "type": "message",
                "id": id,
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{d}", super::super::image_mime(mime).to_string()),
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
        self.out.push(Item {
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
            ts,
        });
    }
    fn push_fallback(
        &mut self,
        block: &Block,
        prefix: &str,
        content_type: &str,
        role: &str,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let id = self.mint(prefix);
        // Intentional fallback: callers only route blocks `fallback_text`
        // handles here, so the default is unreachable in practice.
        let text = fallback_text(block).unwrap_or_default();
        self.out.push(Item {
            wrapper: "response_item",
            payload: json!({
                "type": "message",
                "id": id,
                "role": role,
                "content": [{"type": content_type, "text": text}],
            }),
            thread: Some((
                "agentMessage".into(),
                json!({"type": "agentMessage", "id": id, "text": text}),
            )),
            item_id: id,
            ts,
        });
    }
    fn push_compaction(&mut self, block: &Block, ts: Option<chrono::DateTime<chrono::Utc>>) {
        let id = self.mint("cmp_");
        let text = fallback_text(block).unwrap_or_default();
        self.out.push(Item {
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
            ts,
        });
    }
    fn push_usage(
        &mut self,
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let id = self.mint("usage_");
        self.out.push(Item {
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
            ts,
        });
    }
}
