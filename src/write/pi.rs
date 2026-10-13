//! pi / oh-my-pi writer (shared v3 JSONL schema).
//!
//! Emits native records: multi-part messages (`text`, `thinking`, `toolCall`,
//! `image`), `toolResult` messages carrying their `toolCallId`, compaction
//! records, and per-message usage so the TUI's stats still render.
use super::{import_id, iso, iso_stamp, push, synth_id};
use crate::canonical::{Block, CanonicalSession, Role};
use crate::model::Tool;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Shown as a system message at the top of every import: the transcript that
/// follows already ran somewhere else and must not be treated as live state.
pub(super) const IMPORT_NOTE: &str = "Imported history from another agent's session. \
     The tool calls and results below already executed there; treat them as a transcript to \
     read, never as pending state to continue.";

pub(super) fn write(target: Tool, session: &CanonicalSession) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let is_omp = matches!(target, Tool::Omp);
    // pi wraps the absolute path slug; omp strips home then wraps segments.
    // The store is chosen by the TARGET, not the source.
    let (base, slug) = match target {
        Tool::Pi => (
            home.join(".pi/agent/sessions"),
            // native: `-` + `<cwd>/`.replace('/','-') + `-`  →  `--a-b-c--`
            format!(
                "-{}-",
                format!("{}/", session.cwd.to_string_lossy()).replace('/', "-")
            ),
        ),
        _ => (home.join(".omp/agent/sessions"), {
            let rel = session
                .cwd
                .strip_prefix(&home)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| session.cwd.to_string_lossy().to_string());
            format!("-{}", rel.trim_start_matches('/').replace('/', "-"))
        }),
    };
    let target = base.join(slug).join(format!(
        "{}_{}.jsonl",
        iso_stamp(session.created),
        if is_omp {
            omp_id(session)
        } else {
            import_id(session.tool, &session.id)
        }
    ));
    let sid = if is_omp {
        omp_id(session)
    } else {
        synth_id(session.tool, &session.id)
    };
    let ts = iso(&session.created);
    let ts_ms = session.created.timestamp_millis();
    let title = super::title_for(session);
    let mut buf = Vec::new();
    let mut title_event = json!({
        "type": "title",
        "v": 1,
        "title": title,
        "source": if is_omp { "auto" } else { "xses" },
    });
    if is_omp {
        // OMP validates these fields on the first title record; Pi accepts the
        // smaller title shape.
        title_event["updatedAt"] = json!(ts);
        title_event["pad"] = json!(" ".repeat(127));
    }
    if is_omp {
        push(&mut buf, &title_event);
    }
    let mut session_header = json!({
        "type": "session",
        "version": 3,
        "id": sid,
        "timestamp": ts,
        "cwd": session.cwd,
    });
    if is_omp {
        // omp's resume picker reads the title off the session record
        // (`title: M.title ?? j`); without it the row falls back to the
        // bare first message. Native shape verified on a forked session.
        session_header["title"] = json!(title);
        session_header["titleSource"] = json!("auto");
    } else {
        session_header["title"] = json!(title);
        session_header["titleSource"] = json!("xses");
        session_header["xses"] = json!({
            "source": session.tool.as_str(),
            "srcId": session.id,
            "mapping": crate::write::MAPPING,
        });
    }
    push(&mut buf, &session_header);
    if !is_omp {
        // Pi's resume scanner requires the session header to be the first
        // record; it still discovers this title record immediately after it.
        push(&mut buf, &title_event);
        // pi's picker renders `session.name ?? firstMessage` and ignores the
        // title record/header entirely — the name comes only from a
        // `session_info` entry (`pi --name` writes the same shape).
        push(
            &mut buf,
            &json!({
                "type": "session_info",
                "id": format!("{sid}-nm"),
                "parentId": None::<String>,
                "timestamp": ts,
                "name": title,
            }),
        );
    }
    // Native sessions open with a model + thinking-level chain; pi's TUI
    // renders session stats off these and off per-message `usage` (both
    // verified against a live session — missing usage crashes the renderer).
    let model_id = format!("{sid}-mdl");
    let think_id = format!("{sid}-thk");
    let model_change = if is_omp {
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": ts,
            "model": "xses/imported",
            "role": "default",
            "resolvedModelIsFallback": false,
        })
    } else {
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": ts,
            "provider": "xses",
            "modelId": "imported",
        })
    };
    push(&mut buf, &model_change);
    let thinking_change = if is_omp {
        json!({
            "type": "thinking_level_change",
            "id": think_id,
            "parentId": model_id,
            "timestamp": ts,
            "thinkingLevel": "off",
            "configured": null,
        })
    } else {
        json!({
            "type": "thinking_level_change",
            "id": think_id,
            "parentId": model_id,
            "timestamp": ts,
            "thinkingLevel": "off",
        })
    };
    push(&mut buf, &thinking_change);
    let mut parent: Option<String> = Some(think_id);
    let mut minted = 0usize;
    fn next_id(sid: &str, minted: &mut usize) -> String {
        *minted += 1;
        format!("{sid}-m{:04}", *minted)
    }

    // Import note: the first thing a resumed agent reads.
    {
        let mid = next_id(&sid, &mut minted);
        push(
            &mut buf,
            &json!({
                "type": "message",
                "id": mid,
                "parentId": parent,
                "timestamp": ts,
                "message": {
                    "role": "system",
                    "content": [{"type": "text", "text": IMPORT_NOTE}],
                    "timestamp": ts_ms,
                },
            }),
        );
        parent = Some(mid);
    }

    for (_, blocks) in session.grouped() {
        let role = message_role(&blocks, Role::Assistant);
        let usage = blocks.iter().find_map(|b| match b {
            Block::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                cost,
                model,
            } => Some((
                *input,
                *output,
                *reasoning,
                *cache_read,
                *cache_write,
                *cost,
                model.clone(),
            )),
            _ => None,
        });
        let mut parts: Vec<Value> = Vec::new();
        let mut results: Vec<Value> = Vec::new();
        for block in &blocks {
            match block {
                Block::Text { text, .. } => parts.push(json!({"type": "text", "text": text})),
                Block::Thinking { text, .. } => {
                    if !text.trim().is_empty() {
                        parts.push(json!({"type": "thinking", "thinking": text}));
                    }
                }
                Block::ToolCall {
                    name,
                    args,
                    call_id,
                    ..
                } => {
                    let arguments: Value =
                        serde_json::from_str(args).unwrap_or_else(|_| json!(args));
                    parts.push(json!({
                        "type": "toolCall",
                        "id": call_id.clone().unwrap_or_else(|| format!("call-{minted}-{name}")),
                        "name": name,
                        "arguments": arguments,
                    }));
                }
                Block::ToolResult {
                    name,
                    output,
                    call_id,
                    is_error,
                    duration_ms,
                } => {
                    results.push(json!({
                        "toolCallId": call_id.clone().unwrap_or_else(|| format!("call-{name}")),
                        "toolName": name,
                        "isError": is_error,
                        "durationMs": duration_ms.unwrap_or(0),
                        "content": [{"type": "text", "text": output}],
                    }));
                }
                Block::Image { mime, data, path, bytes } => match data {
                    Some(d) => parts.push(json!({
                        "type": "image",
                        "mimeType": mime.clone().unwrap_or_else(|| "image/png".into()),
                        "data": d,
                    })),
                    None => parts.push(json!({
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
                    parts.push(json!({"type": "text", "text": format!("[context: {kind}]\n{body}")}));
                }
                Block::Patch { files, .. } => parts.push(json!({
                    "type": "text",
                    "text": format!(
                        "[patch: {} file{}]\n{}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" },
                        files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\n")
                    ),
                })),
                Block::Compressed { text } => {
                    parts.push(json!({"type": "text", "text": format!("[compaction]\n{text}")}));
                }
                Block::Gap { detail, .. } => parts.push(json!({
                    "type": "text",
                    "text": format!("[gap in imported history: {detail}]"),
                })),
                Block::Usage { .. } => {}
            }
        }
        if !parts.is_empty() {
            let mid = next_id(&sid, &mut minted);
            let mut message = json!({
                "role": role_str(role),
                "content": parts,
                "timestamp": ts_ms,
            });
            if matches!(role, Role::Assistant) {
                let (input, output, reasoning, cache_read, cache_write, cost, model) =
                    usage.clone().unwrap_or((0, 0, 0, 0, 0, 0.0, None));
                message["api"] = json!("xses");
                message["provider"] = json!("xses");
                message["model"] = json!(model.unwrap_or_else(|| "imported".into()));
                message["stopReason"] = json!("stop");
                message["thinkingLevel"] = json!("off");
                message["durationMs"] = json!(0);
                message["usage"] = json!({
                    "input": input,
                    "output": output,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                    "totalTokens": input + output + reasoning,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost},
                });
            }
            push(
                &mut buf,
                &json!({
                    "type": "message",
                    "id": mid,
                    "parentId": parent,
                    "timestamp": ts,
                    "message": message,
                }),
            );
            parent = Some(mid);
        }
        for result in results {
            let mid = next_id(&sid, &mut minted);
            push(
                &mut buf,
                &json!({
                    "type": "message",
                    "id": mid,
                    "parentId": parent,
                    "timestamp": ts,
                    "message": {
                        "role": "toolResult",
                        "timestamp": ts_ms,
                        "toolCallId": result["toolCallId"],
                        "toolName": result["toolName"],
                        "isError": result["isError"],
                        "durationMs": result["durationMs"],
                        "content": result["content"],
                    },
                }),
            );
            parent = Some(mid);
        }
    }
    if !super::atomic_write_journaled(
        &target,
        &buf,
        &format!("{} {}", session.tool.as_str(), session.id),
    ) {
        return None;
    }
    Some(target)
}

/// The role a group of blocks came from (its text blocks decide).
fn message_role(blocks: &[&Block], fallback: Role) -> Role {
    blocks
        .iter()
        .find_map(|b| match b {
            Block::Text { role, .. } => Some(*role),
            _ => None,
        })
        .unwrap_or(fallback)
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

/// OMP validates session IDs as UUIDv7: the first 48 bits are the millisecond
/// timestamp, followed by the version and RFC 9562 variant. The rest stays
/// deterministic so repeated syncs keep identity.
pub(super) fn omp_id(session: &CanonicalSession) -> String {
    let ms = session.created.timestamp_millis().max(0) as u64;
    let base: String = synth_id(session.tool, &session.id)
        .chars()
        .filter(|c| *c != '-')
        .collect();
    format!(
        "{:08x}-{:04x}-7{:03x}-8{:03x}-{}",
        (ms >> 16) as u32,
        (ms & 0xffff) as u32,
        u16::from_str_radix(&base[0..3], 16).unwrap_or(0) & 0xfff,
        u16::from_str_radix(&base[3..6], 16).unwrap_or(0) & 0xfff,
        &base[6..18]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Block, Event, Role};
    use chrono::{DateTime, Utc};

    #[test]
    fn omp_id_is_timestamped_uuidv7() {
        let created = DateTime::<Utc>::from_timestamp(1_700_000_000, 123_000_000).unwrap();
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Pi,
            cwd: "/tmp".into(),
            created,
            title: None,
            events: vec![],
        };
        let id = omp_id(&session);
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'7');
        assert_eq!(id.as_bytes()[19], b'8');
    }

    #[test]
    fn message_role_follows_text_blocks() {
        let user = Block::Text {
            role: Role::User,
            text: "hi".into(),
        };
        let gap = Block::Gap {
            kind: "image".into(),
            detail: "gone".into(),
        };
        let blocks = vec![&gap, &user];
        assert_eq!(message_role(&blocks, Role::Assistant), Role::User);
        let _ = Event { msg: 0, block: gap };
    }
}
