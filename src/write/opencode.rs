//! opencode writer: synthesize an export payload, pipe it through `opencode import`.
use super::{group_role, import_id, manifest, role_str};
use crate::canonical::{Block, CanonicalSession, Role};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
/// opencode: synthesize an export-format payload and pipe it through
/// `opencode import`. We never touch opencode's SQLite store directly — the
/// CLI owns all schema knowledge (showagent's proven policy).
/// The model the user actually runs in opencode (from the most recent
/// assistant message in its DB), resolved lazily; falls back to the
/// default when the store is empty.
static OPENCODE_MODEL: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    let fallback = json!({"providerID": "opencode", "modelID": "big-pickle", "id": "big-pickle"});
    let query =
        "SELECT json_extract(data,'$.providerID') AS p, json_extract(data,'$.modelID') AS m \
                 FROM message WHERE json_extract(data,'$.role')='assistant' \
                 AND json_extract(data,'$.modelID') IS NOT NULL \
                 ORDER BY time_created DESC LIMIT 1;";
    let Ok(out) = std::process::Command::new("opencode")
        .args(["db", "--format", "json", query])
        .output()
    else {
        return fallback;
    };
    let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return fallback;
    };
    match rows.first() {
        Some(r) => {
            let p = r.get("p").and_then(|v| v.as_str()).unwrap_or("opencode");
            let m = r.get("m").and_then(|v| v.as_str()).unwrap_or("big-pickle");
            // the import validator requires `model.id` as well
            json!({"providerID": p, "modelID": m, "id": m})
        }
        None => fallback,
    }
});

pub(super) fn write(session: &CanonicalSession) -> Option<PathBuf> {
    let ses = format!("ses_{}", import_id(session.tool, &session.id));
    let now_ms = chrono::Utc::now().timestamp_millis();
    let model = local_opencode_model();
    let provider_id = model
        .get("providerID")
        .and_then(|v| v.as_str())
        .unwrap_or("opencode")
        .to_string();
    let model_id = model
        .get("modelID")
        .and_then(|v| v.as_str())
        .unwrap_or("big-pickle")
        .to_string();
    // Native shapes (verified against a live export): assistant infos carry
    // parentID/mode/path/cost/tokens + top-level providerID/modelID, user infos
    // carry agent/model/summary and no parentID.
    let mut prev_id: Option<String> = None;
    let mut msgs_json: Vec<serde_json::Value> = Vec::new();
    let groups = session.grouped();
    let total = groups.len().max(1);
    // opencode keeps a call and its result in ONE `tool` part, so results are
    // folded by call id before the message loop.
    let mut folded: std::collections::HashMap<String, (String, bool, Option<u64>)> =
        std::collections::HashMap::new();
    for (_, blocks) in &groups {
        for block in blocks {
            if let Block::ToolResult {
                call_id,
                output,
                is_error,
                duration_ms,
                name,
            } = block
            {
                folded.insert(
                    call_id.clone().unwrap_or_else(|| format!("call-{name}")),
                    (output.clone(), *is_error, *duration_ms),
                );
            }
        }
    }
    let mut minted = 0usize;
    // Import note rides on the first message as assistant text: opencode's
    // import validator rejects `role: "system"` outright (verified).
    let mut note_pending = true;
    for (i, (_, blocks)) in groups.into_iter().enumerate() {
        // opencode orders messages by (time_created, id): ids must sort in
        // transcript order and timestamps must increase, or the import shows
        // up scrambled.
        let mid = format!("msg_{i:06}-{}", mint_suffix());
        let ts = now_ms - ((total - i) as i64);
        let role = group_role(&blocks, Role::Assistant);
        // opencode's import validator only accepts user/assistant messages
        // ("Expected Message" on role=system, verified against the live CLI),
        // so transcript-context lines ride as assistant text.
        let is_assistant = matches!(role, Role::Assistant | Role::System);
        let usage = blocks.iter().find_map(|b| match b {
            Block::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                cost,
                ..
            } => Some((
                *input,
                *output,
                *reasoning,
                *cache_read,
                *cache_write,
                *cost,
            )),
            _ => None,
        });
        let info = if is_assistant {
            let (input, output, reasoning, cache_read, cache_write, cost) =
                usage.unwrap_or((0, 0, 0, 0, 0, 0.0));
            json!({
                "id": mid,
                "sessionID": ses,
                "role": "assistant",
                "parentID": prev_id.clone().unwrap_or_else(|| format!("msg_{:06}-seed", 0)),
                "time": {"created": ts},
                "mode": "build",
                "agent": "build",
                "path": {"cwd": session.cwd, "root": session.cwd},
                "cost": cost,
                "tokens": {"input": input, "output": output, "reasoning": reasoning,
                           "cache": {"read": cache_read, "write": cache_write}},
                "providerID": provider_id,
                "modelID": model_id,
            })
        } else {
            let mut info = json!({
                "id": mid,
                "sessionID": ses,
                "role": role_str(role),
                "time": {"created": ts},
                "agent": "build",
                "model": model,
                "summary": {"diffs": []},
            });
            if let Some(prev) = &prev_id {
                info["parentID"] = json!(prev);
            }
            info
        };
        let mut parts: Vec<serde_json::Value> = Vec::new();
        fn part_id(minted: &mut usize, prefix: &str) -> String {
            *minted += 1;
            format!("{prefix}{minted:04}-{}", mint_suffix())
        }
        if note_pending {
            note_pending = false;
            let pid = part_id(&mut minted, "prt_");
            parts.push(json!({
                "type": "text",
                "id": pid,
                "sessionID": ses,
                "messageID": mid,
                "text": super::pi::IMPORT_NOTE,
            }));
        }
        for block in &blocks {
            match block {
                Block::Text { text, .. } => {
                    let pid = part_id(&mut minted, "prt_");
                    parts.push(json!({"type": "text", "id": pid, "sessionID": ses, "messageID": mid, "text": text}));
                }
                Block::Thinking { text, .. } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let pid = part_id(&mut minted, "prt_");
                    parts.push(json!({
                        "type": "reasoning",
                        "id": pid,
                        "sessionID": ses,
                        "messageID": mid,
                        "text": text,
                        "time": {"start": ts, "end": ts},
                    }));
                }
                Block::ToolCall {
                    name,
                    args,
                    call_id,
                    status,
                } => {
                    let cid = call_id
                        .clone()
                        .unwrap_or_else(|| format!("call-{minted}-{name}"));
                    // The validator requires an object input and a mandatory
                    // `state.output`; a call whose result was never recorded
                    // keeps an empty output rather than failing the import.
                    let input: serde_json::Value = serde_json::from_str(args)
                        .ok()
                        .filter(|v: &serde_json::Value| v.is_object())
                        .unwrap_or_else(|| json!({"raw": args}));
                    let (output, is_error, duration) = folded
                        .get(&cid)
                        .cloned()
                        .unwrap_or_else(|| (String::new(), false, None));
                    let pid = part_id(&mut minted, "prt_");
                    // The import validator wants the whole union present:
                    // `output` and `error` both, with the unused one null.
                    let mut state = json!({
                        "status": if is_error || status.as_deref() == Some("error") {
                            "error"
                        } else {
                            "completed"
                        },
                        "input": input,
                        "output": if is_error { Value::Null } else { json!(output) },
                        "error": if is_error { json!(output) } else { Value::Null },
                        "title": name,
                        "metadata": {},
                        "time": {"start": ts, "end": ts},
                    });
                    if let Some(d) = duration {
                        state["metadata"]["durationMs"] = json!(d);
                    }
                    parts.push(json!({
                        "type": "tool",
                        "id": pid,
                        "sessionID": ses,
                        "messageID": mid,
                        "tool": name,
                        "callID": cid,
                        "state": state,
                    }));
                }
                // Results were folded into their call's part above.
                Block::ToolResult { .. } => {}
                Block::Patch { hash, files } => {
                    let pid = part_id(&mut minted, "prt_");
                    if hash.as_deref().is_some_and(|h| !h.is_empty()) {
                        parts.push(json!({
                            "type": "patch",
                            "id": pid,
                            "sessionID": ses,
                            "messageID": mid,
                            "hash": hash.clone().unwrap_or_default(),
                            "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        }));
                    } else {
                        parts.push(json!({
                            "type": "text",
                            "id": pid,
                            "sessionID": ses,
                            "messageID": mid,
                            "text": format!(
                                "[patch: {} file{}]\n{}",
                                files.len(),
                                if files.len() == 1 { "" } else { "s" },
                                files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\n")
                            ),
                        }));
                    }
                }
                Block::Image {
                    mime,
                    data,
                    path,
                    bytes,
                } => {
                    let pid = part_id(&mut minted, "prt_");
                    let mime = mime.clone().unwrap_or_else(|| "image/png".to_string());
                    match data {
                        // `file` parts take a data URL (schema: type, mime,
                        // filename, url) — verified against the live importer.
                        Some(d) => {
                            let filename = path
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .and_then(|n| n.to_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    format!("image-{bytes}.{}", extension_for(&mime))
                                });
                            parts.push(json!({
                                "type": "file",
                                "id": pid,
                                "sessionID": ses,
                                "messageID": mid,
                                "mime": mime,
                                "filename": filename,
                                "url": format!("data:{mime};base64,{d}"),
                            }));
                        }
                        None => parts.push(json!({
                            "type": "text",
                            "id": pid,
                            "sessionID": ses,
                            "messageID": mid,
                            "text": format!(
                                "[image: {} ({bytes} bytes) not carried]",
                                path.as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or(mime)
                            ),
                        })),
                    }
                }
                Block::Context { kind, text, path } => {
                    let body = text
                        .clone()
                        .or_else(|| path.as_ref().map(|p| p.display().to_string()))
                        .unwrap_or_default();
                    let pid = part_id(&mut minted, "prt_");
                    parts.push(
                        json!({"type": "text", "id": pid, "sessionID": ses, "messageID": mid,
                                      "text": format!("[context: {kind}]\n{body}")}),
                    );
                }
                Block::Compressed { text } => {
                    let pid = part_id(&mut minted, "prt_");
                    parts.push(
                        json!({"type": "text", "id": pid, "sessionID": ses, "messageID": mid,
                                      "text": format!("[compaction]\n{text}")}),
                    );
                }
                Block::Gap { detail, .. } => {
                    let pid = part_id(&mut minted, "prt_");
                    parts.push(
                        json!({"type": "text", "id": pid, "sessionID": ses, "messageID": mid,
                                      "text": format!("[gap in imported history: {detail}]")}),
                    );
                }
                Block::Usage { .. } => {}
            }
        }
        if parts.is_empty() {
            // Never emit an empty message: opencode's validator rejects it.
            let pid = part_id(&mut minted, "prt_");
            parts.push(json!({"type": "text", "id": pid, "sessionID": ses, "messageID": mid, "text": "[empty message]"}));
        }
        msgs_json.push(json!({"info": info, "parts": parts}));
        prev_id = Some(mid);
    }
    let payload = json!({
        "info": {
            "id": ses,
        "slug": format!("xses-{}", import_id(session.tool, &session.id)),
            "projectID": "global",
            "directory": session.cwd,
            "path": session.cwd.to_string_lossy().trim_start_matches('/'),
            "title": super::title_for(session),
            "version": "1.0.0",
            "time": {"created": now_ms, "updated": now_ms},
            "agent": "build",
            "model": model,
            "summary": {"diffs": [], "additions": 0, "deletions": 0, "files": 0},
            "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "xses": manifest(session),
        },
        "messages": msgs_json,
    });
    let tmp = std::env::temp_dir().join(format!("xses-{ses}.json"));
    let subject = format!("{} {}", session.tool.as_str(), session.id);
    let bytes = match serde_json::to_vec_pretty(&payload) {
        Ok(b) => b,
        Err(e) => {
            super::journal_failure(&subject, &format!("payload serialization: {e}"));
            return None;
        }
    };
    if let Err(e) = fs::write(&tmp, &bytes) {
        super::journal_failure(&subject, &format!("{}: {e}", tmp.display()));
        return None;
    }
    // Concurrent `opencode import` runs are safe (verified 8-way against the
    // live store); only serial `export` truncated stdout, and xses never
    // shells out to export. Imports therefore run parallel across sessions.
    // The CLI's import cost is linear in payload size (~1.8s/MB measured up
    // to 45MB) on top of ~1s of startup, so the deadline scales with it.
    let deadline = Duration::from_secs(60 + 3 * (bytes.len() as u64 / (1024 * 1024)));
    // A failed import is almost always a lost SQLite lock race (the store's
    // WAL grows into the hundreds of MB during a batch). First attempts run
    // in parallel; retries take this lock so only one is in flight, and each
    // waits out the others before trying again.
    static RETRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let mut last = None;
    for attempt in 0..3u32 {
        let _serial;
        if attempt > 0 {
            // opencode opens SQLite with busy_timeout=0, so a competing writer
            // fails immediately instead of waiting; retries back off and run
            // one at a time so only external writers can still be in the way.
            _serial = Some(RETRY_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
            std::thread::sleep(Duration::from_millis(750 * 3u64.pow(attempt - 1)));
        }
        let mut command = Command::new("opencode");
        command
            .arg("import")
            .arg(&tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = crate::proc::output_with_timeout(command, deadline);
        let imported = match &out {
            Ok(o) => {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .any(|line| line.trim_start().starts_with("Imported session:"))
            }
            Err(_) => false,
        };
        if imported {
            fs::remove_file(&tmp).ok();
            return Some(PathBuf::from(format!("opencode://{ses}")));
        }
        last = Some(out);
    }
    let reason = match last {
        Some(Ok(o)) => format!(
            "{} {}",
            o.status,
            String::from_utf8_lossy(&o.stderr)
                .trim()
                .lines()
                .take(4)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        Some(Err(e)) => e.to_string(),
        None => "unknown".into(),
    };
    crate::issues::record("opencode-import", &ses, &reason);
    eprintln!("opencode import failed for {ses}: {reason}");
    fs::remove_file(&tmp).ok();
    None
}
fn local_opencode_model() -> serde_json::Value {
    OPENCODE_MODEL.clone()
}
/// 24 random alphanumerics.
/// File extension for an image mime type, for the `file` part filename.
fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

fn mint_suffix() -> String {
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();
    let mut s = String::new();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as usize;
    for _ in 0..24 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[(seed >> 33) % alphabet.len()]);
    }
    s
}
