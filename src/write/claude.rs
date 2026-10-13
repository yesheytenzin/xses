//! claude writer: transcript + `custom-title.json` sidecar.
//!
//! Emits claude's own content blocks: `thinking`, `text`, `tool_use`, and
//! `tool_result` (the latter in a `user` row, as claude does), plus image
//! blocks for inline pictures and per-message `usage`.
use super::{group_role, iso, manifest, push, synth_id};
use crate::canonical::{Block, CanonicalSession, Role};
use serde_json::{json, Value};
use std::path::PathBuf;

pub(super) fn write(session: &CanonicalSession, revision: Option<&str>) -> Vec<Option<PathBuf>> {
    let Some((rollout, sidecar)) = write_inner(session, revision) else {
        return vec![None];
    };
    // The sidecar is a second artifact clean() must remove.
    vec![Some(rollout), Some(sidecar)]
}

fn write_inner(session: &CanonicalSession, revision: Option<&str>) -> Option<(PathBuf, PathBuf)> {
    let home = dirs::home_dir()?;
    let slug = session.cwd.to_string_lossy().replace('/', "-");
    let sid = synth_id(session.tool, &super::artifact_key(session, revision));
    // Claude's /resume only lists files whose name == the session UUID. Its row
    // label is `agentName || customTitle || aiTitle || summary || firstPrompt`
    // (binary-verified): firstPrompt is the bare first user message, so the
    // [agent] prefix must live in the customTitle sidecar:
    // `<slug>/<sid>/custom-title.json` = `{"customTitle": ...}`.
    let title = super::title_for(session);
    let target = home
        .join(".claude/projects")
        .join(slug)
        .join(format!("{sid}.jsonl"));
    let mut buf = Vec::new();

    // Real uuids: the picker's leafUuid/parent chains must be UUID-shaped.
    let mut uuids: Vec<String> = Vec::with_capacity(session.grouped().len() + 2);
    for i in 0..(session.grouped().len() + 2) {
        uuids.push(uuid_like(&sid, i));
    }
    let mut cursor = 0usize;
    fn next_uuid(cursor: &mut usize, uuids: &[String], sid: &str) -> String {
        let u = uuids
            .get(*cursor)
            .cloned()
            .unwrap_or_else(|| uuid_like(sid, *cursor));
        *cursor += 1;
        u
    }

    push(
        &mut buf,
        &json!({
            "type": "last-prompt",
            "lastPrompt": title,
            "leafUuid": uuids.last().cloned().unwrap_or_else(|| sid.clone()),
            "sessionId": sid,
            "xses": manifest(session),
        }),
    );
    let mut parent: Option<String> = None;

    // Import note first, as a claude `system` row.
    {
        let uuid = next_uuid(&mut cursor, &uuids, &sid);
        push(
            &mut buf,
            &json!({
                "type": "system",
                "message": {"role": "system", "content": super::pi::IMPORT_NOTE},
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": iso(&session.created),
                "cwd": session.cwd,
                "sessionId": sid,
                "isSidechain": false,
                "userType": "external",
            }),
        );
        parent = Some(uuid);
    }

    // Real time when the source recorded one, so the imported timeline is the
    // session's own; rows must not go backwards, so equal-or-older times step
    // forward by a millisecond.
    let mut prev_ts = session.created;
    // Anthropic rejects an id outside `[A-Za-z0-9_-]`; map once so a call and
    // its result keep the same value.
    let mut ids = super::TargetIds::default();
    for (m, blocks) in session.grouped() {
        let row_ts = match session.msg_times.get(&m).copied() {
            Some(ts) if ts > prev_ts => ts,
            _ => prev_ts + chrono::Duration::milliseconds(1),
        };
        prev_ts = row_ts;
        let role = group_role(&blocks, Role::Assistant);
        let mut content: Vec<Value> = Vec::new();
        let mut results: Vec<Value> = Vec::new();
        for block in &blocks {
            match block {
                Block::Text { text, .. } => content.push(json!({"type": "text", "text": text})),
                Block::Thinking { text, .. } => {
                    if !text.trim().is_empty() {
                        content.push(json!({"type": "thinking", "thinking": text}));
                    }
                }
                Block::ToolCall {
                    name,
                    args,
                    call_id,
                    ..
                } => {
                    let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!(args));
                    content.push(json!({
                        "type": "tool_use",
                        "id": call_id
                            .clone()
                            .map(|c| ids.get(&c))
                            .unwrap_or_else(|| format!("toolu_{}", uuid_like(&sid, cursor).replace('-', ""))),
                        "name": name,
                        "input": input,
                    }));
                }
                Block::ToolResult {
                    output,
                    call_id,
                    is_error,
                    ..
                } => results.push(json!({
                    "type": "tool_result",
                    // Anthropic's pattern requires a non-empty
                    // `[A-Za-z0-9_-]+`, so a result whose call id the source
                    // never recorded gets a minted one.
                    "tool_use_id": call_id
                        .clone()
                        .map(|c| ids.get(&c))
                        .unwrap_or_else(|| format!("toolu_{}", uuid_like(&sid, cursor).replace('-', ""))),
                    "is_error": is_error,
                    "content": [{"type": "text", "text": output}],
                })),
                Block::Image { mime, data, path, bytes } => match data {
                    Some(d) => content.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime.clone().unwrap_or_else(|| "image/png".into()),
                            "data": d,
                        },
                    })),
                    None => content.push(json!({
                        "type": "text",
                        "text": format!(
                            "[image reference: {} ({bytes} bytes)]",
                            path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| mime.clone().unwrap_or_else(|| "image".into()))
                        ),
                    })),
                },
                Block::Context { kind, text, path } => {
                    let body = text
                        .clone()
                        .or_else(|| path.as_ref().map(|p| p.display().to_string()))
                        .unwrap_or_default();
                    content.push(json!({"type": "text", "text": format!("[context: {kind}]\n{body}")}));
                }
                Block::Patch { files, .. } => content.push(json!({
                    "type": "text",
                    "text": format!(
                        "[patch: {} file{}]\n{}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\n")
                    ),
                })),
                Block::Compressed { text } => {
                    content.push(json!({"type": "text", "text": format!("[compaction]\n{text}")}));
                }
                Block::Gap { detail, .. } => content.push(json!({
                    "type": "text",
                    "text": format!("[gap in imported history: {detail}]"),
                })),
                Block::Usage { .. } => {}
            }
        }
        let usage = blocks.iter().find_map(|b| match b {
            Block::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                ..
            } => Some((*input, *output, *reasoning, *cache_read, *cache_write)),
            _ => None,
        });

        if !content.is_empty() {
            let uuid = next_uuid(&mut cursor, &uuids, &sid);
            let mut message = json!({"role": super::role_str(role), "content": content});
            if matches!(role, Role::Assistant) {
                let (input, output, reasoning, cache_read, cache_write) =
                    usage.unwrap_or((0, 0, 0, 0, 0));
                message["usage"] = json!({
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_write,
                    "output_tokens_details": {"thinking_tokens": reasoning},
                });
            }
            push(
                &mut buf,
                &json!({
                    "type": if matches!(role, Role::System) { "system" } else { super::role_str(role) },
                    "message": message,
                    "uuid": uuid,
                    "parentUuid": parent,
                    "timestamp": iso(&row_ts),
                    "cwd": session.cwd,
                    "sessionId": sid,
                    "isSidechain": false,
                    "userType": "external",
                }),
            );
            parent = Some(uuid);
        }
        if !results.is_empty() {
            // claude keeps tool results in a user row.
            let uuid = next_uuid(&mut cursor, &uuids, &sid);
            push(
                &mut buf,
                &json!({
                    "type": "user",
                    "message": {"role": "user", "content": results},
                    "uuid": uuid,
                    "parentUuid": parent,
                    "timestamp": iso(&row_ts),
                    "cwd": session.cwd,
                    "sessionId": sid,
                    "isSidechain": false,
                    "userType": "external",
                }),
            );
            parent = Some(uuid);
        }
    }
    let subject = format!("{} {}", session.tool.as_str(), session.id);
    if !super::atomic_write_journaled(&target, &buf, &subject) {
        return None;
    }
    // Sidecar must exist or the picker row shows the bare first prompt.
    let sidecar_dir = target.with_extension("");
    let sidecar = sidecar_dir.join("custom-title.json");
    let sidecar_bytes = match serde_json::to_vec(&json!({"customTitle": title})) {
        Ok(b) => b,
        Err(e) => {
            super::journal_failure(&subject, &format!("title sidecar: {e}"));
            return None;
        }
    };
    if !super::atomic_write_journaled(&sidecar, &sidecar_bytes, &subject) {
        if revision.is_some() {
            let _ = std::fs::remove_file(&target);
        }
        return None;
    }
    Some((target, sidecar))
}

/// Deterministic UUID-shaped id derived from the session id + index.
fn uuid_like(seed: &str, i: usize) -> String {
    let mut h = [0x811c9dc5u32; 4];
    let key = format!("{seed}:{i}");
    for (idx, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[idx % 4] ^= b;
        h[idx % 4] = h[idx % 4].wrapping_mul(0x01000193);
    }
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        h[0],
        h[1] & 0xffff,
        h[2] & 0xfff,
        h[3] & 0xfff,
        (((h[1] as u64) << 32) | h[0] as u64) & 0xffff_ffff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Block, Event, Role};
    use crate::model::Tool;
    use chrono::{TimeZone, Utc};

    /// Anthropic validates `tool_use.id` against `^[A-Za-z0-9_-]+$` and 400s
    /// otherwise; sources compose ids like `call_x|fc_y`, so the writer must
    /// map both sides of a pair onto the same accepted value.
    #[test]
    fn composite_tool_ids_are_mapped_for_the_api() {
        let _serial = super::super::HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scratch = std::env::temp_dir().join(format!("xses-claude-ids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();

        let composite =
            "call_01a1349e7cfa7468a35787224ed8fc45|fc_01a1349e7cfa7468a35787224ed8fc45".to_string();
        let source_time = Utc.with_ymd_and_hms(2026, 10, 13, 9, 31, 0).unwrap();
        let mut session = CanonicalSession {
            id: "01a1349e-571e-727e-b6e4-221162373935".into(),
            tool: Tool::Omp,
            cwd: "/tmp/project".into(),
            created: Utc.with_ymd_and_hms(2026, 10, 13, 9, 30, 0).unwrap(),
            title: None,
            msg_times: Default::default(),
            events: vec![
                Event {
                    msg: 0,
                    block: Block::ToolCall {
                        name: "bash".into(),
                        args: r#"{"command":"ls"}"#.into(),
                        call_id: Some(composite.clone()),
                        status: None,
                    },
                },
                Event {
                    msg: 1,
                    block: Block::ToolResult {
                        name: "bash".into(),
                        output: "ok".into(),
                        call_id: Some(composite.clone()),
                        is_error: false,
                        duration_ms: Some(5),
                    },
                },
            ],
        };
        session.msg_times.insert(1, source_time);

        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &scratch);
        let produced = write(&session, None);
        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        let files: Vec<&PathBuf> = produced.iter().flatten().collect();
        let rollout = std::fs::read_to_string(files[0]).unwrap();
        let mut seen_call = None;
        let mut seen_result = None;
        let mut result_ts = None;
        for line in rollout.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let content = v.pointer("/message/content");
            let Some(items) = content.and_then(|c| c.as_array()) else {
                continue;
            };
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => seen_call = item["id"].as_str().map(str::to_string),
                    Some("tool_result") => {
                        seen_result = item["tool_use_id"].as_str().map(str::to_string);
                        result_ts = v["timestamp"].as_str().map(str::to_string);
                    }
                    _ => {}
                }
            }
        }
        let call = seen_call.expect("tool_use written");
        assert_eq!(
            Some(call.as_str()),
            seen_result.as_deref(),
            "both sides of the pair must agree"
        );
        assert!(
            call.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "id must satisfy Anthropic's character class: {call}"
        );
        assert_eq!(
            result_ts.as_deref(),
            Some("2026-10-13T09:31:00.000Z"),
            "the row keeps the source message's own time"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The claude writer's failure mode used to be a silent `None`, which sync
    /// reported as "target wrote nothing". Prove both artifacts land on disk.
    #[test]
    fn writes_a_rollout_and_a_title_sidecar() {
        let _serial = super::super::HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scratch = std::env::temp_dir().join(format!("xses-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();

        let session = CanonicalSession {
            id: "01a1349e-571e-727e-b6e4-221162373935".into(),
            tool: Tool::Omp,
            cwd: "/tmp/project".into(),
            created: Utc.with_ymd_and_hms(2026, 10, 13, 9, 30, 0).unwrap(),
            title: Some("[omp] fix the picker".into()),
            msg_times: Default::default(),
            events: vec![
                Event {
                    msg: 0,
                    block: Block::Text {
                        role: Role::User,
                        text: "hello".into(),
                    },
                },
                Event {
                    msg: 1,
                    block: Block::Text {
                        role: Role::Assistant,
                        text: "hi".into(),
                    },
                },
            ],
        };

        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &scratch);
        let produced = write(&session, None);
        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        let files: Vec<&PathBuf> = produced.iter().flatten().collect();
        assert_eq!(files.len(), 2, "rollout + title sidecar");
        for file in &files {
            assert!(
                file.starts_with(&scratch),
                "wrote outside the redirected home: {}",
                file.display()
            );
            assert!(file.exists(), "missing {}", file.display());
        }
        let rollout = std::fs::read_to_string(files[0]).unwrap();
        assert!(
            rollout.lines().count() >= 3,
            "claude rows missing: {rollout}"
        );
        let sidecar: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[1]).unwrap()).unwrap();
        assert_eq!(sidecar["customTitle"], "[omp] fix the picker");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
