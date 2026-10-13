//! Scan the four native session stores into unified `Session` records.
use crate::extract::text_of;
use crate::model::{Session, Tool};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ScanCache {
    #[serde(default)]
    sessions: Vec<Session>,
}

pub fn scan_all(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    // Reuse per unchanged file rather than per store: any agent the user is
    // actively working in appends to its own transcript continuously, and that
    // must cost one file re-parse, not a full-store rescan.
    let prev: HashMap<PathBuf, Session> = load_cache()
        .unwrap_or(None)
        .map(|c| {
            c.sessions
                .into_iter()
                .filter(|s| !s.tool.is_cli_backed())
                .map(|s| (s.path.clone(), s))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    out.extend(scan_claude(&prev)?);
    out.extend(scan_codex(&prev)?);
    out.extend(scan_pi_family(&prev, Tool::Pi)?);
    out.extend(scan_pi_family(&prev, Tool::Omp)?);
    out.extend(scan_opencode(cwd_filter)?);
    out.sort_by_key(|a| std::cmp::Reverse(a.mtime));
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert((s.tool, s.id.clone())));
    let _ = save_cache(&ScanCache {
        sessions: out.clone(),
    });
    Ok(filter_sessions(out, cwd_filter))
}

fn filter_sessions(mut sessions: Vec<Session>, cwd_filter: Option<&Path>) -> Vec<Session> {
    if let Some(filter) = cwd_filter {
        sessions.retain(|s| s.cwd == filter);
    }
    sessions
}

fn cache_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("no data dir")?
        .join("xses/scan-cache.json"))
}

fn load_cache() -> Result<Option<ScanCache>> {
    let path = cache_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn save_cache(cache: &ScanCache) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn file_meta(path: &Path) -> Result<(DateTime<Utc>, DateTime<Utc>, u64)> {
    let md = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = md
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);
    // creation (birth) time is stable across appends; fall back to mtime
    // on filesystems that don't track it.
    let created = md
        .created()
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or(mtime);
    Ok((mtime, created, md.len()))
}

/// Cached record for an unchanged file. Transcripts are append-only, so an
/// identical (mtime, size) pair means the cached summary is still exact; only
/// the file that changed is re-read. The active session in an agent being used
/// right now is the common case, and it must not force a store-wide re-parse.
fn reused_if_unchanged(prev: &HashMap<PathBuf, Session>, path: &Path) -> Option<Session> {
    let cached = prev.get(path)?;
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok().map(DateTime::<Utc>::from)?;
    (cached.mtime == mtime && cached.size == md.len()).then(|| cached.clone())
}

/// Classify how useful a session is, from what its turns look like.
fn noise_reason(user_turns: u64, assistant_turns: u64, failed_turns: u64) -> Option<String> {
    if user_turns == 0 && assistant_turns == 0 {
        return Some("empty".into());
    }
    if assistant_turns == 0 {
        return Some("unanswered".into());
    }
    if failed_turns == assistant_turns {
        return Some("failed".into());
    }
    None
}

/// Assistant text that is only an error/abort message, checked when the store
/// records no explicit error flag (pi/omp `stopReason`, codex aborts).
fn looks_like_error(text: &str) -> bool {
    let t = text.trim_start();
    const MARKERS: [&str; 8] = [
        "api error",
        "error:",
        "error!",
        "rate limit",
        "overloaded",
        "invalid api key",
        "stream error",
        "connection error",
    ];
    let lower = t.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.starts_with(m))
}

fn truncate(s: &str, max: usize) -> String {
    let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = normalized.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn display_title(tool: Tool, title: &str) -> String {
    let title = truncate(title, 120);
    let known_agent_prefix = ["claude", "codex", "pi", "omp"]
        .iter()
        .any(|agent| title.starts_with(&format!("[{agent}] ")));
    if known_agent_prefix {
        return title;
    }
    if title.is_empty() {
        format!("[{}] (untitled)", tool.as_str())
    } else {
        format!("[{}] {title}", tool.as_str())
    }
}

fn marker_source(v: &serde_json::Value, pointer: &str) -> Option<Tool> {
    v.pointer(pointer)
        .and_then(|s| s.as_str())
        .and_then(Tool::parse)
}

fn prefixed_title_source(title: &str) -> Option<Tool> {
    let end = title.find(']')?;
    Tool::parse(title.get(1..end)?)
}

fn claude_summary(first_user: Option<&str>, first_line: &serde_json::Value) -> String {
    if let Some(u) = first_user {
        truncate(u, 120)
    } else {
        first_line
            .get("summary")
            .and_then(|s| s.as_str())
            .map(|s| truncate(s, 120))
            .unwrap_or_default()
    }
}

fn scan_claude(prev: &HashMap<PathBuf, Session>) -> Result<Vec<Session>> {
    let root = dirs::home_dir()
        .context("no home dir")?
        .join(".claude/projects");
    if !root.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !entry.file_type().is_file() || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(cached) = reused_if_unchanged(prev, p) {
            out.push(cached);
            continue;
        }
        // <slugified-cwd>/<uuid>.jsonl — slug conventions vary, so read the real
        // cwd from the first line that carries one.
        let mut cwd: Option<PathBuf> = None;
        crate::jsonl::for_each_record(p, |v| {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                cwd = Some(PathBuf::from(c));
                return false;
            }
            true
        })?;
        let (mtime, created, size) = match file_meta(p) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let mut messages = 0u64;
        let mut first_user: Option<String> = None;
        let mut imported_marker_seen = false;
        let mut source_tool = None;
        let (mut user_turns, mut asst_turns, mut failed_turns) = (0u64, 0u64, 0u64);
        crate::jsonl::for_each_record(p, |v| {
            if v.get("xses").is_some() || v.get("sessionizer").is_some() {
                imported_marker_seen = true;
                source_tool = marker_source(v, "/xses/source")
                    .or_else(|| marker_source(v, "/sessionizer/source"));
            }
            match v.get("type").and_then(|t| t.as_str()) {
                Some("summary") | Some("last-prompt") => {
                    // these lines carry the real session id they belong to
                    if let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) {
                        if !sid.is_empty() {
                            id = sid.to_string();
                        }
                    }
                }
                Some("user") => {
                    let content = v.get("message").and_then(|m| m.get("content"));
                    let is_meta = v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false);
                    if !is_meta {
                        messages += 1;
                        let text = content.and_then(text_of).filter(|t| !t.trim().is_empty());
                        if text.is_some() {
                            user_turns += 1;
                        }
                        if first_user.is_none() {
                            if let Some(t) = text {
                                if !t.starts_with("<") {
                                    first_user = Some(t);
                                }
                            }
                        }
                    }
                }
                Some("assistant") => {
                    messages += 1;
                    if let Some(t) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(text_of)
                    {
                        if !t.trim().is_empty() {
                            asst_turns += 1;
                            let is_api_error = v
                                .get("isApiErrorMessage")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false);
                            if is_api_error || looks_like_error(&t) {
                                failed_turns += 1;
                            }
                        }
                    }
                }
                Some("system") => messages += 1,
                _ => {}
            }
            true
        })?;
        if id.is_empty() {
            continue;
        }
        out.push(Session {
            id,
            tool: Tool::Claude,
            cwd: cwd.unwrap_or_default(),
            mtime,
            created,
            size,
            messages,
            summary: display_title(
                source_tool.unwrap_or(Tool::Claude),
                &claude_summary(first_user.as_deref(), &serde_json::Value::Null),
            ),
            path: p.to_path_buf(),
            is_import: imported_marker_seen,
            noise: noise_reason(user_turns, asst_turns, failed_turns),
        });
    }
    Ok(out)
}

fn scan_codex(prev: &HashMap<PathBuf, Session>) -> Result<Vec<Session>> {
    let root = dirs::home_dir()
        .context("no home dir")?
        .join(".codex/sessions");
    if !root.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .min_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !entry.file_type().is_file()
            || !p
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        {
            continue;
        }
        if let Some(cached) = reused_if_unchanged(prev, p) {
            out.push(cached);
            continue;
        }
        let (mtime, created, size) = match file_meta(p) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut id = String::new();
        let mut cwd: Option<PathBuf> = None;
        let mut messages = 0u64;
        let mut first_user: Option<String> = None;
        // imports carry the provenance marker in line 1's session_meta
        // (`cli_version`); canonical filenames now look native.
        let mut file_is_import = false;
        let mut source_tool = None;
        let (mut user_turns, mut asst_turns, mut failed_turns) = (0u64, 0u64, 0u64);
        crate::jsonl::for_each_record(p, |v| {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("session_meta") => {
                    id = v
                        .pointer("/payload/id")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string();
                    cwd = v
                        .pointer("/payload/cwd")
                        .and_then(|s| s.as_str())
                        .map(PathBuf::from);
                    file_is_import = v
                        .pointer("/payload/cli_version")
                        .and_then(|c| c.as_str())
                        .map(|c| c == "0.0.0-xses" || c == "0.0.0-sessionizer")
                        .unwrap_or(false);
                    source_tool = marker_source(v, "/payload/xses/source")
                        .or_else(|| marker_source(v, "/payload/sessionizer/source"));
                }
                Some("response_item")
                    if v.pointer("/payload/type").and_then(|t| t.as_str()) == Some("message") =>
                {
                    let role = v
                        .pointer("/payload/role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    if role == "user" {
                        messages += 1;
                        if first_user.is_none() {
                            if let Some(c) = v.pointer("/payload/content") {
                                if let Some(t) = text_of(c) {
                                    if !t.trim().is_empty() {
                                        first_user = Some(t);
                                        user_turns += 1;
                                    }
                                }
                            }
                        } else if v.pointer("/payload/content").and_then(text_of).is_some() {
                            user_turns += 1;
                        }
                    } else if role == "assistant" {
                        messages += 1;
                        if let Some(t) = v.pointer("/payload/content").and_then(text_of) {
                            if !t.trim().is_empty() {
                                asst_turns += 1;
                                if v.pointer("/payload/isApiErrorMessage")
                                    .and_then(|b| b.as_bool())
                                    .unwrap_or(false)
                                    || looks_like_error(&t)
                                {
                                    failed_turns += 1;
                                }
                            }
                        }
                    } else if role == "system" {
                        messages += 1;
                    }
                }
                _ => {}
            }
            true
        })?;
        if id.is_empty() {
            continue;
        }
        out.push(Session {
            id,
            tool: Tool::Codex,
            cwd: cwd.unwrap_or_default(),
            mtime,
            created,
            size,
            messages,
            summary: display_title(
                source_tool.unwrap_or(Tool::Codex),
                first_user.as_deref().unwrap_or_default(),
            ),
            path: p.to_path_buf(),
            is_import: file_is_import,
            noise: noise_reason(user_turns, asst_turns, failed_turns),
        });
    }
    Ok(out)
}

/// pi and oh-my-pi share the v3 JSONL schema; one scanner covers both.
fn scan_pi_family(prev: &HashMap<PathBuf, Session>, tool: Tool) -> Result<Vec<Session>> {
    let base = match tool {
        Tool::Pi => ".pi/agent/sessions",
        Tool::Omp => ".omp/agent/sessions",
        _ => unreachable!(),
    };
    let root = dirs::home_dir().context("no home dir")?.join(base);
    if !root.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .min_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !entry.file_type().is_file() || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(cached) = reused_if_unchanged(prev, p) {
            out.push(cached);
            continue;
        }
        // Slug formats differ (pi wraps full path, omp strips home) — read the
        // real cwd from the session header line instead.
        let mut cwd: Option<PathBuf> = None;
        let mut header_import = false;
        let mut source_tool = None;
        crate::jsonl::for_each_record(p, |v| {
            if v.get("type").and_then(|t| t.as_str()) == Some("title")
                && matches!(
                    v.get("source").and_then(|s| s.as_str()),
                    Some("xses") | Some("sessionizer")
                )
            {
                header_import = true;
                source_tool = v
                    .get("title")
                    .and_then(|t| t.as_str())
                    .and_then(prefixed_title_source);
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                    cwd = Some(PathBuf::from(c));
                }
                header_import |= v.get("xses").is_some() || v.get("sessionizer").is_some();
                source_tool = source_tool
                    .or_else(|| marker_source(v, "/xses/source"))
                    .or_else(|| marker_source(v, "/sessionizer/source"));
                return false;
            }
            true
        })?;
        let (mtime, created, size) = match file_meta(p) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut id = String::new();
        let mut messages = 0u64;
        let mut first_user: Option<String> = None;
        let mut title: Option<String> = None;
        let (mut user_turns, mut asst_turns, mut failed_turns) = (0u64, 0u64, 0u64);
        crate::jsonl::for_each_record(p, |v| {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("session") => {
                    id = v
                        .get("id")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string();
                }
                Some("title") => {
                    if let Some(t) = v.get("title").and_then(|t| t.as_str()) {
                        if !t.trim().is_empty() {
                            title = Some(t.to_string());
                        }
                    }
                }
                Some("message") => {
                    let role = v
                        .pointer("/message/role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    match role {
                        "user" => {
                            messages += 1;
                            if let Some(c) = v.pointer("/message/content") {
                                if let Some(t) = text_of(c).filter(|t| !t.trim().is_empty()) {
                                    user_turns += 1;
                                    if first_user.is_none() {
                                        first_user = Some(t);
                                    }
                                }
                            }
                        }
                        "assistant" => {
                            messages += 1;
                            if let Some(t) = v.pointer("/message/content").and_then(text_of) {
                                if !t.trim().is_empty() {
                                    asst_turns += 1;
                                    let stop = v
                                        .pointer("/message/stopReason")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("stop");
                                    if matches!(stop, "error" | "aborted" | "signal")
                                        || looks_like_error(&t)
                                    {
                                        failed_turns += 1;
                                    }
                                }
                            }
                        }
                        "system" | "developer" => messages += 1,
                        _ => {}
                    }
                }
                _ => {}
            }
            true
        })?;
        if id.is_empty() {
            continue;
        }
        out.push(Session {
            id,
            tool,
            cwd: cwd.unwrap_or_default(),
            mtime,
            created,
            size,
            messages,
            summary: display_title(
                source_tool.unwrap_or(tool),
                title.or(first_user).as_deref().unwrap_or_default(),
            ),
            path: p.to_path_buf(),
            is_import: header_import,
            noise: noise_reason(user_turns, asst_turns, failed_turns),
        });
    }
    Ok(out)
}

/// opencode DB location (respects OPencode_DATA_DIR like the CLI).
pub fn opencode_db() -> Option<std::path::PathBuf> {
    for var in ["OPENCODE_DATA_DIR", "OPencode_DATA_DIR"] {
        if let Ok(dir) = std::env::var(var) {
            let p = std::path::PathBuf::from(dir).join("opencode.db");
            if p.exists() {
                return Some(p);
            }
        }
    }
    let p = dirs::data_dir()?.join("opencode/opencode.db");
    p.exists().then_some(p)
}

/// Return OpenCode session IDs whose durable slug marks them as xses
/// imports. The OpenCode CLI list output does not include `slug`, so cleanup
/// must consult the database directly.
pub fn opencode_import_ids() -> Result<HashSet<String>> {
    let Some(db) = opencode_db() else {
        return Ok(HashSet::new());
    };
    let q = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(&db)
        .arg("SELECT id FROM session WHERE slug LIKE 'xses-%' OR slug LIKE 'sessionizer-%';")
        .output()
        .context("scan OpenCode import markers")?;
    if !q.status.success() {
        anyhow::bail!(
            "OpenCode import scan failed: {}",
            String::from_utf8_lossy(&q.stderr).trim()
        );
    }
    let rows = crate::proc::json_rows(
        &String::from_utf8_lossy(&q.stdout),
        "OpenCode import markers",
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.get("id").and_then(|id| id.as_str()).map(str::to_string))
        .collect())
}

/// opencode: discovery via read-only SQL. `opencode export` truncates stdout
/// under load (64KB chunks, exit 0), so every opencode *read* goes straight to
/// SQLite; only *writes* go through the CLI (`opencode import`). Mirrors the
/// CLI list rule (verified): sessions whose directory equals the cwd when one
/// is pinned, else global+unarchived+root sessions, subagents hidden.
fn scan_opencode(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    let Some(db) = opencode_db() else {
        return Ok(vec![]);
    };
    if std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_err()
    {
        anyhow::bail!("cannot scan OpenCode sessions: sqlite3 is not installed");
    }
    let sql = match cwd_filter {
        Some(f) => format!(
            "SELECT json_object('id', s.id, 'slug', s.slug, 'title', s.title, 'directory', s.directory, 'created', s.time_created, 'messages', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role') IN ('user','assistant','system')), 'user_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='user'), 'asst_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='assistant'), 'ok_asst_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='assistant' AND json_extract(m.data,'$.error') IS NULL)) AS j \
             FROM session s WHERE s.directory = '{}' ORDER BY s.time_created DESC;",
            f.to_string_lossy().replace('\'', "''")
        ),
        None => "SELECT json_object('id', s.id, 'slug', s.slug, 'title', s.title, 'directory', s.directory, 'created', s.time_created, 'messages', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role') IN ('user','assistant','system')), 'user_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='user'), 'asst_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='assistant'), 'ok_asst_turns', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role')='assistant' AND json_extract(m.data,'$.error') IS NULL)) AS j \
             FROM session s WHERE s.time_archived IS NULL \
             AND (s.parent_id IS NULL OR s.parent_id = '') ORDER BY s.time_created DESC;"
            .to_string(),
    };
    let q = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(&db)
        .arg(&sql)
        .output();
    let q = q.context("launch sqlite3 for OpenCode session scan")?;
    if !q.status.success() {
        anyhow::bail!(
            "OpenCode session scan failed: {}",
            String::from_utf8_lossy(&q.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&q.stdout);
    let rows = crate::proc::json_rows(&text, "OpenCode session scan output")?;
    let mut out = Vec::new();
    for r in rows {
        let Ok(obj) = r
            .get("j")
            .and_then(|j| j.as_str())
            .unwrap_or("{}")
            .parse::<serde_json::Value>()
        else {
            continue;
        };
        let id = obj
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let dir = obj
            .get("directory")
            .and_then(|s| s.as_str())
            .map(PathBuf::from);
        let created_ms = obj.get("created").and_then(|c| c.as_i64()).unwrap_or(0);
        let mtime =
            chrono::DateTime::<Utc>::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now);
        let title = obj
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let slug = obj.get("slug").and_then(|s| s.as_str()).unwrap_or_default();
        let messages = obj
            .get("messages")
            .and_then(|m| m.as_i64())
            .unwrap_or(0)
            .max(0) as u64;
        out.push(Session {
            id: id.clone(),
            tool: Tool::Opencode,
            cwd: dir.unwrap_or_default(),
            mtime,
            created: mtime,
            size: 0,
            messages,
            summary: display_title(Tool::Opencode, &title),
            path: PathBuf::from(format!("opencode://{id}")),
            // Titles are user-editable and can legitimately begin with an
            // agent-looking prefix. The slug is written by xses and is
            // the durable provenance marker.
            is_import: slug.starts_with("xses-") || slug.starts_with("sessionizer-"),
            noise: {
                let user = obj.get("user_turns").and_then(|v| v.as_u64()).unwrap_or(0);
                let asst = obj.get("asst_turns").and_then(|v| v.as_u64()).unwrap_or(0);
                let ok = obj
                    .get("ok_asst_turns")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                noise_reason(user, asst, asst.saturating_sub(ok))
            },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_titles_include_agent_and_fit_one_line() {
        assert_eq!(display_title(Tool::Pi, "hello\nworld"), "[pi] hello world");
        assert_eq!(
            display_title(Tool::Opencode, "[claude] imported"),
            "[claude] imported"
        );
        assert_eq!(display_title(Tool::Claude, ""), "[claude] (untitled)");
    }

    #[test]
    fn noise_is_classified_by_turn_shape() {
        // nothing was ever said
        assert_eq!(noise_reason(0, 0, 0).as_deref(), Some("empty"));
        // the user asked, nothing answered
        assert_eq!(noise_reason(1, 0, 0).as_deref(), Some("unanswered"));
        // answered, but only by errors
        assert_eq!(noise_reason(2, 3, 3).as_deref(), Some("failed"));
        // a real conversation
        assert_eq!(noise_reason(2, 3, 1), None);
        assert_eq!(noise_reason(0, 1, 0), None);
        assert!(looks_like_error(
            "API Error: 400 All target providers failed."
        ));
        assert!(looks_like_error("  Rate limit reached"));
        assert!(!looks_like_error("I'll gather system diagnostics first."));
    }

    #[test]
    fn imported_marker_preserves_source_agent() {
        let marker = serde_json::json!({"xses": {"source": "claude"}});
        assert_eq!(marker_source(&marker, "/xses/source"), Some(Tool::Claude));
        assert_eq!(
            display_title(
                marker_source(&marker, "/xses/source").unwrap_or(Tool::Pi),
                "fix the parser"
            ),
            "[claude] fix the parser"
        );
    }

    #[test]
    fn unchanged_files_reuse_cached_sessions() {
        let dir = std::env::temp_dir().join(format!("xses-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        let (mtime, created, size) = file_meta(&path).unwrap();
        let cached = Session {
            id: "id".into(),
            tool: Tool::Pi,
            cwd: "/tmp".into(),
            mtime,
            created,
            size,
            messages: 1,
            summary: "[pi] test".into(),
            path: path.clone(),
            is_import: false,
            noise: None,
        };
        let mut prev: HashMap<PathBuf, Session> = HashMap::new();
        prev.insert(path.clone(), cached);
        assert!(reused_if_unchanged(&prev, &path).is_some());
        // any append changes the stamp and forces a re-parse
        std::fs::write(&path, b"{}\n{}\n").unwrap();
        assert!(reused_if_unchanged(&prev, &path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
