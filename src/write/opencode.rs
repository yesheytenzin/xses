//! opencode writer: synthesize an export payload, pipe it through `opencode import`.
use super::{first_ask, import_id, role_str};
use crate::canonical::{CanonicalSession, Role};
use serde_json::json;
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

pub(super) fn write(session: &CanonicalSession, msgs: &[(Role, String)]) -> Option<PathBuf> {
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
    let mut msgs_json: Vec<serde_json::Value> = Vec::with_capacity(msgs.len());
    for (i, (role, text)) in msgs.iter().enumerate() {
        // opencode orders messages by (time_created, id): ids must sort in
        // transcript order and timestamps must increase, or the import shows
        // up scrambled.
        let mid = format!("msg_{i:06}-{}", mint_suffix());
        let ts = now_ms - ((msgs.len() - i) as i64);
        // opencode's import validator only accepts user/assistant messages
        // ("Expected Message" on role=system, verified against the live CLI),
        // so transcript-context lines ride as assistant text.
        let is_assistant = matches!(role, Role::Assistant | Role::System);
        let info = if is_assistant {
            json!({
                "id": mid,
                "sessionID": ses,
                "role": "assistant",
                "parentID": prev_id.clone().unwrap_or_else(|| format!("msg_{:06}-seed", 0)),
                "time": {"created": ts},
                "mode": "build",
                "agent": "build",
                "path": {"cwd": session.cwd, "root": session.cwd},
                "cost": 0,
                "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "providerID": provider_id,
                "modelID": model_id,
            })
        } else {
            let mut info = json!({
                "id": mid,
                "sessionID": ses,
                "role": role_str(*role),
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
        msgs_json.push(json!({
            "info": info,
            "parts": [{
                "type": "text",
                "id": mint_id("prt_"),
                "sessionID": ses,
                "messageID": mid,
                "text": text,
            }],
        }));
        prev_id = Some(mid);
    }
    let payload = json!({
        "info": {
            "id": ses,
        "slug": format!("xses-{}", import_id(session.tool, &session.id)),
            "projectID": "global",
            "directory": session.cwd,
            "path": session.cwd.to_string_lossy().trim_start_matches('/'),
            "title": format!("[{}] {}", session.tool.as_str(), first_ask(msgs)),
            "version": "1.0.0",
            "time": {"created": now_ms, "updated": now_ms},
            "agent": "build",
            "model": model,
            "summary": {"diffs": [], "additions": 0, "deletions": 0, "files": 0},
            "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "xses": {"source": session.tool.as_str(), "srcId": session.id},
        },
        "messages": msgs_json,
    });
    let tmp = std::env::temp_dir().join(format!("xses-{ses}.json"));
    let bytes = serde_json::to_vec_pretty(&payload).ok()?;
    fs::write(&tmp, &bytes).ok()?;
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
    for attempt in 0..2 {
        let _serial;
        if attempt > 0 {
            _serial = Some(RETRY_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
            std::thread::sleep(Duration::from_millis(1000));
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
fn mint_id(prefix: &str) -> String {
    format!("{prefix}{}", mint_suffix())
}
/// 24 random alphanumerics.
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
