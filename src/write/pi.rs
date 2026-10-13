//! pi / oh-my-pi writer (shared v3 JSONL schema).
use super::{atomic_write, first_ask, import_id, iso, iso_stamp, push, role_str, synth_id};
use crate::canonical::CanonicalSession;
use crate::model::Tool;
use serde_json::json;
use std::path::PathBuf;
pub(super) fn write(
    target: Tool,
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<PathBuf> {
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
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(msgs));
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
    for (i, (role, text)) in msgs.iter().enumerate() {
        let mid = format!("{sid}-m{i:04}");
        let mut message = json!({
            "role": role_str(*role),
            "content": [{"type": "text", "text": text}],
            "timestamp": ts_ms,
        });
        if matches!(role, crate::canonical::Role::Assistant) {
            message["api"] = json!("xses");
            message["provider"] = json!("xses");
            message["model"] = json!("imported");
            message["stopReason"] = json!("stop");
            message["thinkingLevel"] = json!("off");
            message["durationMs"] = json!(0);
            message["usage"] = json!({
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
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
    atomic_write(&target, &buf).ok()?;
    Some(target)
}
pub(super) fn omp_id(session: &CanonicalSession) -> String {
    // OMP validates session IDs as UUIDv7: the first 48 bits are the
    // millisecond timestamp, followed by the version and RFC 9562 variant.
    // Keep the remaining bits deterministic so repeated syncs retain identity.
    let base: String = synth_id(session.tool, &session.id)
        .chars()
        .filter(|c| *c != '-')
        .collect();
    let timestamp = (session.created.timestamp_millis() as u64) & 0x0000_ffff_ffff_ffff;
    let mut hex = format!("{timestamp:012x}{}", &base[12..]);
    let mut chars: Vec<char> = hex.drain(..).collect();
    chars[12] = '7';
    chars[16] = '8';
    format!(
        "{}-{}-{}-{}-{}",
        chars[0..8].iter().collect::<String>(),
        chars[8..12].iter().collect::<String>(),
        chars[12..16].iter().collect::<String>(),
        chars[16..20].iter().collect::<String>(),
        chars[20..32].iter().collect::<String>()
    )
}
