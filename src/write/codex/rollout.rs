//! codex rollout records: IR blocks → codex item vocabulary.
//!
//! [`Roller`] threads id state through one transcript block at a time so
//! rollout ordinals stay aligned with the TUI projection.
use super::super::next_row_time;
use super::roller::Roller;
use super::Item;
use crate::canonical::CanonicalSession;

/// Map the IR onto codex's item vocabulary, keeping one entry per rollout
/// record so ordinals stay aligned with the projection.
pub(super) fn rollout_items(session: &CanonicalSession) -> Vec<Item> {
    let mut roller = Roller {
        out: Vec::new(),
        n: 0,
        pending: Vec::new(),
        ids: super::super::TargetIds::default(),
        cwd: &session.cwd,
    };
    roller.push_import_note();
    let mut prev_ts = session.created;
    for (m, blocks) in session.grouped() {
        let group_ts = next_row_time(session, m, &mut prev_ts);
        for block in &blocks {
            roller.push_block(block, Some(group_ts));
        }
    }
    roller.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Block, Event};
    use crate::model::Tool;
    use chrono::Utc;
    use serde_json::Value;

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
    fn composite_call_ids_meet_on_the_mapped_value() {
        // codex/omp compose `call_x|fc_y`: both sides of the pair must land
        // on the target-safe `call_x` — a raw `|` id 500s nothing here but
        // trips the SQL allowlist at seed time (verified live).
        let s = test_session(vec![
            Event {
                msg: 0,
                block: Block::ToolCall {
                    name: "read".into(),
                    args: r#"{"path":"/tmp/x"}"#.into(),
                    call_id: Some("call_abc|fc_abc".into()),
                    status: None,
                },
            },
            Event {
                msg: 1,
                block: Block::ToolResult {
                    name: "read".into(),
                    output: "a".into(),
                    call_id: Some("call_abc|fc_abc".into()),
                    is_error: false,
                    duration_ms: None,
                },
            },
        ]);
        let items = rollout_items(&s);
        let ids: Vec<&str> = items.iter().map(|i| i.item_id.as_str()).collect();
        assert!(
            ids.iter().all(|id| !id.contains('|')),
            "raw composite id leaked: {ids:?}"
        );
        let call = items
            .iter()
            .find(|i| i.payload.get("type").and_then(|t| t.as_str()) == Some("function_call"))
            .expect("call row");
        let result = items
            .iter()
            .find(|i| {
                i.payload.get("type").and_then(|t| t.as_str()) == Some("function_call_output")
            })
            .expect("result row");
        assert_eq!(
            call.payload.get("call_id").and_then(|v| v.as_str()),
            result.payload.get("call_id").and_then(|v| v.as_str()),
            "call and result must share one id"
        );
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
                provider: None,
            },
        }]);
        let items = rollout_items(&s);
        assert_eq!(items.len(), 2); // note + usage
        assert_eq!(items[1].thread, None);
        assert_eq!(items[1].wrapper, "event_msg");
    }
}
