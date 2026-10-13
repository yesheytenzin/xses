//! Extract: source transcript → IR (`CanonicalSession`).
//!
//! One extractor per source tool. Sources are strictly read-only. Everything a
//! source holds is captured: tool calls and results with their correlation ids,
//! reasoning (with its signature when present), injected context, images,
//! patches, compaction summaries, and token/cost accounting. Anything that
//! cannot be captured becomes a named gap rather than disappearing.
use crate::canonical::CanonicalSession;
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

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


/// Turn a discovered session into its IR: extract the transcript, then keep
/// the scan's title when the source had one.
pub fn canonicalize(s: &crate::model::Session) -> Result<CanonicalSession> {
    let mut canon = extract(s.tool, &s.id, &s.cwd, s.created, &s.path)?;
    canon.title = Some(s.summary.clone()).filter(|t| !t.trim().is_empty());
    Ok(canon)
}

/// Hash the selected source's actual rows, not just session timestamps. This
/// catches same-length edits and deletions by clients that omit revision bumps.
pub fn opencode_revision(id: &str) -> Result<(String, u64)> {
    use std::hash::Hasher;
    let db =
        crate::scan::opencode_db().ok_or_else(|| anyhow::anyhow!("opencode database not found"))?;
    let id = id.replace('\'', "''");
    // Both generations; the absent one contributes no rows. `session_v2`
    // probing keeps older stores (v1 tables only) working.
    let v2 = if crate::scan::opencode_has_v2(&db) {
        format!(" UNION ALL SELECT 'session_v2' AS kind, id, json_object('title', title, 'directory', directory, 'created', time_created) AS data FROM session_v2 WHERE id='{id}'
        UNION ALL SELECT 'session_message', id, json_object('type', type, 'data', data, 'created', time_created) FROM session_message WHERE session_id='{id}'")
    } else {
        String::new()
    };
    let sql = format!("SELECT 'session' AS kind, id, json_object('title', title, 'directory', directory, 'created', time_created) AS data FROM session WHERE id='{id}'
        UNION ALL SELECT 'message', id, json_object('data', data, 'created', time_created) FROM message WHERE session_id='{id}'
        UNION ALL SELECT 'part', id, json_object('data', data, 'created', time_created) FROM part WHERE session_id='{id}'{v2} ORDER BY kind, id;");
    let mut cmd = std::process::Command::new("sqlite3");
    cmd.args(["-readonly", "-json"]).arg(db).arg(sql);
    let out = crate::proc::output_with_timeout(cmd, std::time::Duration::from_secs(60))?;
    anyhow::ensure!(
        out.status.success(),
        "OpenCode revision read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    hash.write(&out.stdout);
    Ok((format!("{:016x}", hash.finish()), out.stdout.len() as u64))
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

mod claude;
mod codex;
mod common;
mod opencode;
mod pi;

use claude::claude;
use codex::codex;
use opencode::opencode;
use pi::pi_family;

#[cfg(test)]
mod tests {
    use super::claude::{claude, claude_attachment};
    use super::common::{codex_compaction, collect_parts};
    use super::opencode::{opencode_part, opencode_v2_content};
    use super::text_of;
    use crate::canonical::{Block, Event, Events, Role};
    use crate::model::Tool;
    use serde_json::Value;
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

    #[test]
    fn opencode_v2_tool_states_split_completed_and_error() {
        // Completed tools carry `state.content[]`; error tools carry
        // `state.error.message` instead (shapes read from a live v2 store).
        let mut out = Events::default();
        opencode_v2_content(
            &json!({
                "type": "tool", "id": "call-1", "name": "bash",
                "state": {"status": "completed", "input": {"command": "ls"},
                          "content": [{"type": "text", "text": "ok"}]},
            }),
            &mut out,
        );
        opencode_v2_content(
            &json!({
                "type": "tool", "id": "call-2", "name": "read",
                "state": {"status": "error", "input": {"filePath": "x"},
                          "error": {"type": "tool.execution", "message": "gone"}},
            }),
            &mut out,
        );
        let events = out.into_vec();
        assert!(matches!(&events[0].block, Block::ToolCall { name, .. } if name == "bash"));
        match &events[1].block {
            Block::ToolResult { output, is_error, .. } => {
                assert_eq!(output, "ok");
                assert!(!is_error);
            }
            other => panic!("want ToolResult, got {other:?}"),
        }
        match &events[3].block {
            Block::ToolResult { output, is_error, .. } => {
                assert_eq!(output, "gone");
                assert!(is_error);
            }
            other => panic!("want ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn opencode_v2_text_and_reasoning_keep_roles() {
        let mut out = Events::default();
        opencode_v2_content(&json!({"type": "text", "text": "hi"}), &mut out);
        opencode_v2_content(
            &json!({"type": "reasoning", "text": "why"}),
            &mut out,
        );
        let events = out.into_vec();
        assert!(matches!(&events[0].block, Block::Text { .. }));
        assert!(matches!(&events[1].block, Block::Thinking { .. }));
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
    fn claude_persisted_tool_outputs_are_restored() {
        // Shape from a live store: the row points at a file holding the full
        // output claude cut down to a 2 KB preview in the transcript.
        let dir = std::env::temp_dir().join(format!("xses-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("by0bn2k8o.txt");
        std::fs::write(&sidecar, "line one\nline two\n").unwrap();
        let row = json!({
            "type": "user",
            "timestamp": "2026-10-13T09:32:00Z",
            "toolUseResult": {
                "persistedOutputPath": sidecar.to_str().unwrap(),
                "persistedOutputSize": 18,
                "stdout": "",
                "stderr": "",
            },
            "message": {"role": "user", "content": "<persisted-output>\nOutput too large. Preview follows"}
        });
        let events = claude_rows(&[row]);
        match &events[0].block {
            Block::Context { kind, text, path } => {
                assert_eq!(kind, "tool-output");
                let text = text.as_deref().unwrap_or("");
                assert!(text.contains("line two"), "full text carried: {text}");
                assert!(text.contains("restored"), "provenance kept: {text}");
                assert!(path.is_some(), "the sidecar path is recorded");
            }
            other => panic!("want Context, got {other:?}"),
        }
        // A sidecar that is gone leaves the transcript's own text alone.
        let row = json!({
            "type": "user",
            "toolUseResult": {"persistedOutputPath": "/nonexistent/gone.txt"},
            "message": {"role": "user", "content": "preview only"}
        });
        let events = claude_rows(&[row]);
        assert!(
            matches!(&events[0].block, Block::Text { text, .. } if text.contains("preview only")),
            "fall back to the transcript text: {:?}",
            events[0].block
        );
        let _ = std::fs::remove_dir_all(&dir);
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
