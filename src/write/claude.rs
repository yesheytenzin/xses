//! claude writer: transcript + `custom-title.json` sidecar.
//!
//! Emits claude's own content blocks: `thinking`, `text`, `tool_use`, and
//! `tool_result` (the latter in a `user` row, as claude does), plus image
//! blocks for inline pictures and per-message `usage`.
use super::{atomic_write, first_ask, group_role, iso, manifest, push, synth_id};
use crate::canonical::{Block, CanonicalSession, Role};
use serde_json::{json, Value};
use std::path::PathBuf;

pub(super) fn write(session: &CanonicalSession) -> Vec<Option<PathBuf>> {
    let Some((rollout, sidecar)) = write_inner(session) else {
        return vec![None];
    };
    // The sidecar is a second artifact clean() must remove.
    vec![Some(rollout), Some(sidecar)]
}

fn write_inner(session: &CanonicalSession) -> Option<(PathBuf, PathBuf)> {
    let home = dirs::home_dir()?;
    let slug = session.cwd.to_string_lossy().replace('/', "-");
    let sid = synth_id(session.tool, &session.id);
    // Claude's /resume only lists files whose name == the session UUID. Its row
    // label is `agentName || customTitle || aiTitle || summary || firstPrompt`
    // (binary-verified): firstPrompt is the bare first user message, so the
    // [agent] prefix must live in the customTitle sidecar:
    // `<slug>/<sid>/custom-title.json` = `{"customTitle": ...}`.
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(session));
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

    for (_, blocks) in session.grouped() {
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
                        "id": call_id.clone().unwrap_or_else(|| format!("toolu_{}", uuid_like(&sid, cursor).replace('-', ""))),
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
                    "tool_use_id": call_id.clone().unwrap_or_default(),
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
                    "timestamp": iso(&session.created),
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
                    "timestamp": iso(&session.created),
                    "cwd": session.cwd,
                    "sessionId": sid,
                    "isSidechain": false,
                    "userType": "external",
                }),
            );
            parent = Some(uuid);
        }
    }
    atomic_write(&target, &buf).ok()?;
    // Sidecar must exist or the picker row shows the bare first prompt.
    let sidecar_dir = target.with_extension("");
    std::fs::create_dir_all(&sidecar_dir).ok()?;
    let sidecar = sidecar_dir.join("custom-title.json");
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&json!({"customTitle": title})).ok()?,
    )
    .ok()?;
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
