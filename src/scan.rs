//! Scan the four native session stores into unified `Session` records.
use crate::extract::text_of;
use crate::model::{Session, Tool};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileStamp {
    path: PathBuf,
    modified_ns: u128,
    size: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ScanCache {
    files: Vec<FileStamp>,
    opencode_db: Vec<FileStamp>,
    #[serde(default)]
    index: Option<FileStamp>,
    sessions: Vec<Session>,
}

pub fn scan_all(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    let imported = imported_paths();
    let inventory = native_inventory(&imported)?;
    let db_stamp = opencode_inventory();
    let index_stamp = xses_index().and_then(|p| file_stamp(&p).ok());
    if let Some(cache) = load_cache().unwrap_or(None) {
        if cache.files == inventory && cache.opencode_db == db_stamp && cache.index == index_stamp {
            return Ok(filter_sessions(cache.sessions, cwd_filter));
        }
    }
    let sessions = scan_all_uncached(None)?;
    let _ = save_cache(&ScanCache {
        files: inventory,
        opencode_db: db_stamp,
        index: index_stamp,
        sessions: sessions.clone(),
    });
    Ok(filter_sessions(sessions, cwd_filter))
}

fn scan_all_uncached(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    let mut out = Vec::new();
    out.extend(scan_claude(cwd_filter)?);
    out.extend(scan_codex(cwd_filter)?);
    out.extend(scan_pi_family(cwd_filter, Tool::Pi)?);
    out.extend(scan_pi_family(cwd_filter, Tool::Omp)?);
    out.extend(scan_opencode(cwd_filter)?);
    out.sort_by_key(|a| std::cmp::Reverse(a.mtime));
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert((s.tool, s.id.clone())));
    Ok(out)
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

fn file_stamp(path: &Path) -> Result<FileStamp> {
    let md = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let modified_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(FileStamp {
        path: path.to_path_buf(),
        modified_ns,
        size: md.len(),
    })
}

fn native_inventory(imported: &HashSet<PathBuf>) -> Result<Vec<FileStamp>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(vec![]);
    };
    let roots = [
        home.join(".claude/projects"),
        home.join(".codex/sessions"),
        home.join(".pi/agent/sessions"),
        home.join(".omp/agent/sessions"),
    ];
    let mut files = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
                && !imported.contains(entry.path())
            {
                if let Ok(stamp) = file_stamp(entry.path()) {
                    files.push(stamp);
                }
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn xses_index() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("xses").join("index.jsonl"))
}

/// Imported files are already represented by the index. Excluding them from
/// the native inventory prevents every sync write from invalidating the scan
/// cache and forcing a full transcript walk.
fn imported_paths() -> HashSet<PathBuf> {
    let Some(path) = xses_index() else {
        return HashSet::new();
    };
    let Ok(file) = std::fs::File::open(path) else {
        return HashSet::new();
    };
    let mut paths = HashSet::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Some((_, encoded)) = line.split_once('\t') else {
            continue;
        };
        let Ok(values) = serde_json::from_str::<Vec<String>>(encoded) else {
            continue;
        };
        paths.extend(
            values
                .into_iter()
                .filter_map(|p| (!p.starts_with("opencode://")).then_some(PathBuf::from(p))),
        );
    }
    paths
}

fn opencode_inventory() -> Vec<FileStamp> {
    let Some(db) = opencode_db() else {
        return Vec::new();
    };
    [db.clone(), db.with_extension("db-wal")]
        .into_iter()
        .filter_map(|path| file_stamp(&path).ok())
        .collect()
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

/// Read a JSONL file, invoking `f` per parsed line; stops early via `f` returning `false`.
fn for_each_line(path: &Path, mut f: impl FnMut(&serde_json::Value) -> bool) -> Result<u64> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut n = 0u64;
    loop {
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if line.trim().is_empty() {
            line.clear();
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => v,
            // A transcript can contain a permanently truncated historical
            // record. Stop indexing that file at the bad record; extraction
            // remains strict and will reject it if the user selects it for
            // sync, while ordinary listings stay quiet.
            Err(_) => break,
        };
        n += 1;
        if !f(&v) {
            break;
        }
        line.clear();
    }
    Ok(n)
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

fn scan_claude(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
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
        // <slugified-cwd>/<uuid>.jsonl — slug conventions vary, so read the real
        // cwd from the first line that carries one.
        let mut cwd: Option<PathBuf> = None;
        for_each_line(p, |v| {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                cwd = Some(PathBuf::from(c));
                return false;
            }
            true
        })?;
        if let Some(f) = cwd_filter {
            if cwd.as_deref() != Some(f) {
                continue;
            }
        }
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
        for_each_line(p, |v| {
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
                        if first_user.is_none() {
                            if let Some(t) = content.and_then(text_of) {
                                if !t.trim().is_empty() && !t.starts_with("<") {
                                    first_user = Some(t);
                                }
                            }
                        }
                    }
                }
                Some("assistant") | Some("system") => messages += 1,
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
        });
    }
    Ok(out)
}

fn scan_codex(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
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
        for_each_line(p, |v| {
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
                                    }
                                }
                            }
                        }
                    } else if matches!(role, "assistant" | "system") {
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
        if let Some(f) = cwd_filter {
            if cwd.as_deref() != Some(f) {
                continue;
            }
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
        });
    }
    Ok(out)
}

/// pi and oh-my-pi share the v3 JSONL schema; one scanner covers both.
fn scan_pi_family(cwd_filter: Option<&Path>, tool: Tool) -> Result<Vec<Session>> {
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
        // Slug formats differ (pi wraps full path, omp strips home) — read the
        // real cwd from the session header line instead.
        let mut cwd: Option<PathBuf> = None;
        let mut header_import = false;
        let mut source_tool = None;
        for_each_line(p, |v| {
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
        if let Some(f) = cwd_filter {
            if cwd.as_deref() != Some(f) {
                continue;
            }
        }
        let (mtime, created, size) = match file_meta(p) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut id = String::new();
        let mut messages = 0u64;
        let mut first_user: Option<String> = None;
        let mut title: Option<String> = None;
        for_each_line(p, |v| {
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
                            if first_user.is_none() {
                                if let Some(c) = v.pointer("/message/content") {
                                    if let Some(t) = text_of(c) {
                                        if !t.trim().is_empty() {
                                            first_user = Some(t);
                                        }
                                    }
                                }
                            }
                        }
                        "assistant" | "system" => messages += 1,
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
        if let Some(f) = cwd_filter {
            if cwd.as_deref() != Some(f) {
                continue;
            }
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
    if q.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(HashSet::new());
    }
    let rows = serde_json::from_slice::<Vec<serde_json::Value>>(&q.stdout)
        .context("parse OpenCode import markers")?;
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
            "SELECT json_object('id', s.id, 'slug', s.slug, 'title', s.title, 'directory', s.directory, 'created', s.time_created, 'messages', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role') IN ('user','assistant','system'))) AS j \
             FROM session s WHERE s.directory = '{}' ORDER BY s.time_created DESC;",
            f.to_string_lossy().replace('\'', "''")
        ),
        None => "SELECT json_object('id', s.id, 'slug', s.slug, 'title', s.title, 'directory', s.directory, 'created', s.time_created, 'messages', (SELECT count(*) FROM message m WHERE m.session_id = s.id AND json_extract(m.data,'$.role') IN ('user','assistant','system'))) AS j \
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
    let rows = serde_json::from_str::<Vec<serde_json::Value>>(text.trim())
        .context("parse OpenCode session scan output")?;
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
}
