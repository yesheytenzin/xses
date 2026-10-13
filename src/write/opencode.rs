//! opencode writer: synthesize an export payload, pipe it through
//! `opencode session import` (v2; v1 CLI past tense). Reads go straight to
//! SQLite across both store generations; only writes go through the CLI.
//! The v2 import validator takes the `session export` shape: flat messages
//! with inline content, `info.location` + `projectID`, and provenance in
//! `info.metadata.xses` (slugs are server-generated, so the v1 slug marker
//! cannot survive an import).
use super::{
    args_json_object, fallback_call_id, fallback_text, group_role, import_id, manifest, usage_of,
    Usage,
};
use crate::canonical::{Block, CanonicalSession, Role};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
/// opencode: synthesize a v2 export-format payload and pipe it through
/// `opencode session import`. We never touch opencode's SQLite store directly
/// — the CLI owns all schema knowledge (showagent's proven policy).
/// The model the user actually runs in opencode (from the most recent
/// assistant message in its DB, either store generation), resolved lazily;
/// falls back to the default when the store is empty.
static OPENCODE_MODEL: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    let fallback = json!({"providerID": "opencode", "modelID": "big-pickle", "id": "big-pickle"});
    // v2 keeps the model nested (`model.{id,providerID}` on assistant rows);
    // v1 kept `providerID`/`modelID` beside the role. Newest row wins.
    for query in [
        "SELECT json_extract(data,'$.model.providerID') AS p, json_extract(data,'$.model.id') AS m \
         FROM session_message WHERE type='assistant' \
         AND json_extract(data,'$.model.id') IS NOT NULL \
         ORDER BY time_created DESC LIMIT 1;",
        "SELECT json_extract(data,'$.providerID') AS p, json_extract(data,'$.modelID') AS m \
         FROM message WHERE json_extract(data,'$.role')='assistant' \
         AND json_extract(data,'$.modelID') IS NOT NULL \
         ORDER BY time_created DESC LIMIT 1;",
    ] {
        let Some(db) = crate::scan::opencode_db() else {
            break;
        };
        let Ok(rows) = crate::proc::sqlite_json(
            &db,
            query,
            std::time::Duration::from_secs(30),
            "opencode model lookup"
        ) else {
            continue;
        };
        if let Some(r) = rows.first() {
            let p = r.get("p").and_then(|v| v.as_str()).unwrap_or("opencode");
            let m = r.get("m").and_then(|v| v.as_str()).unwrap_or("big-pickle");
            // the import validator requires `model.id` as well
            return json!({"providerID": p, "modelID": m, "id": m});
        }
    }
    fallback
});

/// v2 message builder: folds tool results, then renders one flat row per
/// source group. Carries the id mapping minted fallbacks share.
/// One source group plus its row identity: what every message builder needs.
#[derive(Clone, Copy)]
struct Row<'a> {
    mid: &'a str,
    ts: i64,
    end_ts: i64,
    role: Role,
    blocks: &'a [&'a Block],
}

struct Builder {
    ids: super::TargetIds,
    minted: usize,
    folded: std::collections::HashMap<String, (String, bool, Option<u64>)>,
    model_id: String,
    provider_id: String,
}

impl Builder {
    /// Calls and results meet in one `tool` block, so results fold by call
    /// id before the message loop. Minted fallback ids are shared with the
    /// loop so an id-less call and its result still meet.
    fn fold(&mut self, groups: &[(u32, Vec<&Block>)]) {
        for (_, blocks) in groups {
            for block in blocks {
                if let Block::ToolResult {
                    call_id,
                    output,
                    is_error,
                    duration_ms,
                    name,
                } = block
                {
                    self.folded.insert(
                        fallback_call_id(&mut self.ids, call_id, &mut self.minted, name),
                        (output.clone(), *is_error, *duration_ms),
                    );
                }
            }
        }
    }

    fn build_content(
        &mut self,
        row: Row,
        take_note: bool,
    ) -> (Vec<Value>, Vec<Value>, Option<Value>) {
        let Row {
            ts,
            end_ts,
            role,
            blocks,
            ..
        } = row;
        let mut content: Vec<Value> = Vec::new();
        let mut files: Vec<Value> = Vec::new();
        let mut note = None;
        if take_note {
            if matches!(role, Role::User) {
                // The note is assistant speech: it gets its own row ahead of
                // a leading user message so roles stay clean.
                note = Some(json!({
                    "id": "msg_000000-note",
                    "type": "assistant",
                    "agent": "build",
                    "model": {"id": self.model_id, "providerID": self.provider_id},
                    "content": [{"type": "text", "text": super::IMPORT_NOTE}],
                    "time": {"created": ts, "completed": end_ts},
                    "finish": "stop",
                    "cost": 0,
                    "tokens": {"input": 0, "output": 0, "reasoning": 0,
                               "cache": {"read": 0, "write": 0}},
                }));
            } else {
                content.push(json!({"type": "text", "text": super::IMPORT_NOTE}));
            }
        }
        for block in blocks {
            match block {
                Block::Text { text, .. } => {
                    content.push(json!({"type": "text", "text": text}));
                }
                Block::Thinking { text, .. } => {
                    if !text.trim().is_empty() {
                        content.push(json!({
                            "type": "reasoning",
                            "text": text,
                            "time": {"created": ts, "completed": end_ts},
                        }));
                    }
                }
                Block::ToolCall {
                    name,
                    args,
                    call_id,
                    status,
                } => {
                    // Same mapping the fold loop used, so the call meets its
                    // self.folded result even when the source id needed remapping.
                    let cid = fallback_call_id(&mut self.ids, call_id, &mut self.minted, name);
                    // Completed states carry `content[]`; error states carry
                    // `error.message` instead — each shape without the other
                    // key (verified against live rows and the validator).
                    let input = args_json_object(args);
                    let (output, is_error, duration) = self
                        .folded
                        .get(&cid)
                        .cloned()
                        .unwrap_or_else(|| (String::new(), false, None));
                    let is_error = is_error || status.as_deref() == Some("error");
                    let state = if is_error {
                        json!({"status": "error", "input": input,
                           "error": {"type": "tool.execution", "message": output}})
                    } else {
                        json!({"status": "completed", "input": input,
                           "content": [{"type": "text", "text": output}]})
                    };
                    let call_end = ts + duration.unwrap_or(1).max(1) as i64;
                    content.push(json!({
                        "type": "tool",
                        "id": cid,
                        "name": name,
                        "state": state,
                        "time": {"created": ts, "completed": call_end.max(end_ts)},
                    }));
                }
                // Results were self.folded into their call's part above.
                Block::ToolResult { .. } => {}
                Block::Image {
                    mime,
                    data,
                    path,
                    bytes,
                } => {
                    match data {
                        // User rows carry pictures as file entries
                        // (data/mime/source/name, verified live).
                        Some(d) if matches!(role, Role::User) => {
                            let mime = mime.clone().unwrap_or_else(|| "image/png".to_string());
                            let filename = path
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .and_then(|n| n.to_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    format!("image-{bytes}.{}", extension_for(&mime))
                                });
                            files.push(json!({
                                "data": d,
                                "mime": mime,
                                "source": {"type": "inline"},
                                "name": filename,
                            }));
                        }
                        _ => {
                            if let Some(text) = fallback_text(block) {
                                content.push(json!({"type": "text", "text": text}));
                            }
                        }
                    }
                }
                Block::Context { .. }
                | Block::Patch { .. }
                | Block::Compressed { .. }
                | Block::Gap { .. } => {
                    if let Some(text) = fallback_text(block) {
                        content.push(json!({"type": "text", "text": text}));
                    }
                }
                Block::Usage { .. } => {}
            }
        }
        (content, files, note)
    }

    fn assemble(
        &self,
        row: Row,
        mut content: Vec<Value>,
        files: Vec<Value>,
        usage: &Usage,
    ) -> Value {
        let Row {
            mid,
            ts,
            end_ts,
            role,
            blocks,
        } = row;
        // Plain-text view of this group's content, for message types that
        // carry a single `text` instead of blocks.
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|c| {
                if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                    c.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        let joined = texts.join("\n\n");
        let msg = if matches!(role, Role::User) {
            let mut m = json!({
                "id": mid,
                "type": "user",
                "text": if joined.is_empty() { "[empty message]".into() } else { joined },
                "time": {"created": ts},
            });
            if !files.is_empty() {
                m["files"] = json!(files);
            }
            m
        } else if matches!(role, Role::System) {
            json!({
                "id": mid,
                "type": "system",
                "text": if joined.is_empty() { "[empty message]".into() } else { joined },
                "time": {"created": ts},
            })
        } else if !content.is_empty()
            && blocks.iter().all(|b| matches!(b, Block::Compressed { .. }))
        {
            // A pure compaction group rides as a real compaction message.
            json!({
                "id": mid,
                "type": "compaction",
                "status": "completed",
                "reason": "auto",
                "summary": joined,
                "recent": "",
                "time": {"created": ts},
            })
        } else {
            if content.is_empty() {
                // Never emit an empty assistant: the validator drops it.
                content.push(json!({"type": "text", "text": "[empty message]"}));
            }
            json!({
                "id": mid,
                "type": "assistant",
                "agent": "build",
                "model": {"id": self.model_id, "providerID": self.provider_id},
                "content": content,
                "time": {"created": ts, "completed": end_ts},
                "finish": "stop",
                "cost": usage.cost,
                "tokens": {"input": usage.input, "output": usage.output,
                           "reasoning": usage.reasoning,
                           "cache": {"read": usage.cache_read, "write": usage.cache_write}},
            })
        };
        msg
    }
}

/// Installed opencode major version (`opencode --version` → `v2.0.28`).
/// `None` means undetectable — callers proceed as before.
static OPENCODE_MAJOR: std::sync::LazyLock<Option<u32>> = std::sync::LazyLock::new(|| {
    let mut cmd = std::process::Command::new("opencode");
    cmd.arg("--version");
    let out = crate::proc::output_with_timeout(cmd, std::time::Duration::from_secs(10)).ok()?;
    parse_cli_major(&String::from_utf8_lossy(&out.stdout))
});

/// First version-like token wins: `v2.0.28` → 2, `1.12.4` → 1.
fn parse_cli_major(out: &str) -> Option<u32> {
    out.split_whitespace().find_map(|tok| {
        tok.strip_prefix('v')
            .unwrap_or(tok)
            .split('.')
            .next()?
            .parse()
            .ok()
    })
}

pub(super) fn write(session: &CanonicalSession, revision: Option<&str>) -> Option<PathBuf> {
    if *OPENCODE_MAJOR == Some(1) {
        // The v1 CLI cannot take v2 payloads; say so instead of failing
        // inside its schema validator.
        let detail =
            "opencode 1.x detected: imports need opencode 2+, or xses 0.6.0 for 1.x stores";
        super::journal_failure(&format!("{} {}", session.tool.as_str(), session.id), detail);
        eprintln!("opencode import skipped: {detail}");
        return None;
    }
    let ses = format!(
        "ses_{}",
        import_id(session.tool, &super::artifact_key(session, revision))
    );
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
    // Native shapes (verified against a live `session export`): user rows
    // carry text + files, assistant rows carry inline content plus
    // message-level tokens/cost/model and a mandatory `time.completed`.
    let mut msgs_json: Vec<serde_json::Value> = Vec::new();
    let groups = session.grouped();
    let total = groups.len().max(1);
    let mut builder = Builder {
        ids: super::TargetIds::default(),
        minted: 0,
        folded: std::collections::HashMap::new(),
        model_id,
        provider_id,
    };
    builder.fold(&groups);
    // Import note rides on the first message as assistant text, so a resumed
    // agent reads the transcript as history rather than live state.
    let mut note_pending = true;
    let mut prev_ms = 0i64;
    for (i, (m, blocks)) in groups.into_iter().enumerate() {
        // opencode orders messages by (time_created, id): ids must sort in
        // transcript order and timestamps must increase, or the import shows
        // up scrambled.
        let mid = format!("msg_{i:06}-{}", mint_suffix());
        // The session's own time when the source recorded one; opencode
        // orders by (time_created, id), so times must increase.
        let ts = session
            .msg_times
            .get(&m)
            .map(|t| t.timestamp_millis())
            .filter(|ms| *ms > prev_ms)
            .unwrap_or_else(|| (now_ms - ((total - i) as i64)).max(prev_ms + 1));
        prev_ms = ts;
        let role = group_role(&blocks, Role::Assistant);
        let usage = usage_of(&blocks);
        // Assistant rows require `time.completed`; without it the validator
        // silently drops the message (verified live). End-of-row + 1ms keeps
        // times ordered without claiming false precision — it may equal the
        // next row's `created`, which the import accepts.
        let end_ts = ts + 1;
        let take_note = note_pending;
        note_pending = false;
        let row = Row {
            mid: &mid,
            ts,
            end_ts,
            role,
            blocks: &blocks,
        };
        let (content, files, note) = builder.build_content(row, take_note);
        if let Some(note) = note {
            msgs_json.push(note);
        }
        msgs_json.push(builder.assemble(row, content, files, &usage));
    }
    // v2 import shape (verified against a live `session export`): flat
    // messages with inline content; provenance in `metadata.xses` because
    // slugs are server-generated and cannot carry the marker.
    let title = super::title_for(session);
    let payload = json!({
        "info": {
            "id": ses,
            "location": {"directory": session.cwd},
            "title": title,
            "projectID": "global",
            "cost": 0,
            "time": {"created": now_ms, "updated": now_ms},
            "tokens": {"input": 0, "output": 0, "reasoning": 0,
                       "cache": {"read": 0, "write": 0}},
            "agent": "build",
            "model": {"id": builder.model_id, "providerID": builder.provider_id},
            "metadata": {"xses": manifest(session)},
        },
        "messages": msgs_json,
    });
    let tmp = std::env::temp_dir().join(format!("xses-{}-{ses}.json", std::process::id()));
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
    // Imports are serial per OpenCode store. The deadline scales with payload
    // size; retries only contend with external OpenCode writers.
    let deadline = Duration::from_secs(60 + 3 * (bytes.len() as u64 / (1024 * 1024)));
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
        // The payload directory is ignored: the session lands in the
        // `--directory` cwd, so the source project must be passed outright.
        let mut command = session_cmd();
        command
            .arg("import")
            .arg("--directory")
            .arg(&session.cwd)
            .arg(&tmp);
        let out = crate::proc::output_with_timeout(command, deadline);
        let stdout = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        // Ids are deterministic, so a re-import of an unchanged session is
        // already there: drop it and import fresh so a mapping bump or new
        // turns actually replace the stored copy.
        if stdout.contains("already exists") {
            let mut del = session_cmd();
            del.arg("delete").arg(&ses);
            let del_out = crate::proc::output_with_timeout(del, Duration::from_secs(60));
            if del_out.is_ok_and(|o| o.status.success()) {
                // Fall through to the attempt below, which re-runs the
                // import against the now-absent id.
                last = Some(out);
                continue;
            }
            last = Some(out);
            continue;
        }
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
    // A partially imported generation must not replace the last committed one.
    if revision.is_some() {
        if let Err(e) = crate::sync::retire_artifacts(&[PathBuf::from(format!("opencode://{ses}"))])
        {
            crate::issues::record("opencode-rollback", &ses, &e.to_string());
        }
    }
    let _ = fs::remove_file(&tmp);
    crate::issues::record("opencode-import", &ses, &reason);
    eprintln!(
        "opencode import failed for {ses}: {}",
        crate::ansi::strip(&reason)
    );
    fs::remove_file(&tmp).ok();
    None
}
/// One `opencode session …` invocation with piped output. Callers add
/// their subcommand: imports also pass `--directory` (the payload directory
/// is ignored by the CLI, so the project must be stated outright).
fn session_cmd() -> Command {
    let mut command = Command::new("opencode");
    command
        .arg("session")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub(crate) fn current_model() -> serde_json::Value {
    OPENCODE_MODEL.clone()
}

fn local_opencode_model() -> serde_json::Value {
    current_model()
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

/// 24 random alphanumerics. Counter-seeded: the clock alone repeats within
/// a tick, and only the message index prefix would keep those ids unique.
static MINT_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn mint_suffix() -> String {
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();
    let mut s = String::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mut seed = nanos.wrapping_add(MINT_SEED.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    for _ in 0..24 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[((seed >> 33) % alphabet.len() as u64) as usize]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_major_reads_both_version_styles() {
        assert_eq!(parse_cli_major("opencode v2.0.28\n"), Some(2));
        assert_eq!(parse_cli_major("1.12.4"), Some(1));
        assert_eq!(parse_cli_major("opencode version 2.1.0"), Some(2));
        assert_eq!(parse_cli_major(""), None);
        assert_eq!(parse_cli_major("no version here"), None);
    }
}
