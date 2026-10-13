//! Scan the four native session stores into unified `Session` records.
use crate::model::{Session, Tool};
use crate::text::shorten;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

mod files;

pub fn scan_all(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    scan_tools(cwd_filter, Tool::all_tools(), true)
}

/// Cache files are per store: a filtered scan never evicts another store.
pub fn scan_tools(
    cwd_filter: Option<&Path>,
    tools: &[Tool],
    save_cache: bool,
) -> Result<Vec<Session>> {
    let scan = |tool: Tool| {
        if tool == Tool::Opencode {
            scan_opencode(cwd_filter)
        } else {
            files::scan(tool, save_cache)
        }
    };
    let mut out = if let [tool] = tools {
        scan(*tool)?
    } else {
        std::thread::scope(|threads| -> Result<Vec<Session>> {
            let workers: Vec<_> = tools
                .iter()
                .map(|&tool| threads.spawn(move || scan(tool)))
                .collect();
            let mut sessions = Vec::new();
            for worker in workers {
                sessions.extend(
                    worker
                        .join()
                        .map_err(|_| anyhow::anyhow!("store scanner panicked"))??,
                );
            }
            Ok(sessions)
        })?
    };
    if let Some(cwd) = cwd_filter {
        out.retain(|s| s.cwd == cwd);
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
    let mut seen = HashSet::new();
    out.retain(|s| seen.insert((s.tool, s.id.clone())));
    Ok(out)
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

fn display_title(tool: Tool, title: &str) -> String {
    let title = shorten(title, 120);
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

fn marker_tool(v: &serde_json::Value, pointer: &str) -> Option<Tool> {
    v.pointer(pointer)
        .and_then(|s| s.as_str())
        .and_then(Tool::parse)
}

fn prefixed_title_source(title: &str) -> Option<Tool> {
    let end = title.find(']')?;
    Tool::parse(title.get(1..end)?)
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

/// Return OpenCode session IDs xses imported, across both store generations:
/// v1 marks them with an `xses-*` slug, v2 with `metadata.xses` (slugs are
/// server-generated there). The OpenCode CLI list output exposes neither, so
/// cleanup consults the database directly.
pub fn opencode_import_ids(only: Option<&HashSet<(Tool, String)>>) -> Result<HashSet<String>> {
    let Some(db) = opencode_db() else {
        return Ok(HashSet::new());
    };
    let run = |sql: &str| -> Result<Vec<serde_json::Value>> {
        crate::proc::sqlite_json(&db, sql, std::time::Duration::from_secs(60), "OpenCode import markers")
    };
    let mut rows = run("SELECT id, slug, NULL AS src, NULL AS src_id FROM session WHERE slug LIKE 'xses-%' OR slug LIKE 'sessionizer-%';")?;
    if opencode_has_v2(&db) {
        // The id is deterministic (`ses_` + import hash), so v2 rows can be
        // matched without trusting the editable title.
        rows.extend(run("SELECT id, slug, json_extract(metadata,'$.xses.source') AS src, json_extract(metadata,'$.xses.srcId') AS src_id FROM session_v2 WHERE json_extract(metadata,'$.xses.srcId') IS NOT NULL;")?);
    }
    Ok(rows
        .into_iter()
        .filter(|row| {
            only.is_none_or(|sources| {
                let slug = row.get("slug").and_then(|s| s.as_str()).unwrap_or_default();
                let id = row.get("id").and_then(|s| s.as_str()).unwrap_or_default();
                // v2 rows carry their source inline; v1 rows match by slug/id.
                if let (Some(src), Some(src_id)) = (
                    row.get("src").and_then(|s| s.as_str()),
                    row.get("src_id").and_then(|s| s.as_str()),
                ) {
                    return sources.iter().any(|(tool, source)| {
                        tool.as_str() == src && source == src_id
                    });
                }
                sources.iter().any(|(tool, source)| {
                    let base = crate::write::import_id_for(*tool, source);
                    slug == format!("xses-{base}")
                        || slug == format!("sessionizer-{base}")
                        || id == format!("ses_{base}")
                })
            })
        })
        .filter_map(|row| row.get("id").and_then(|id| id.as_str()).map(str::to_string))
        .collect())
}

/// opencode: discovery via read-only SQL. `opencode export` truncates stdout
/// under load (64KB chunks, exit 0), so every opencode *read* goes straight to
/// SQLite; only *writes* go through the CLI (`opencode session import`).
/// Mirrors the CLI list rule (verified): sessions whose directory equals the
/// cwd when one is pinned, else global+unarchived+root sessions, subagents
/// hidden. Both store generations are read: v1 (`session`/`message`) and v2
/// (`session_v2`/`session_message`) — v2 no longer lists v1 rows, but they
/// are still real sessions xses may have written.
fn scan_opencode(cwd_filter: Option<&Path>) -> Result<Vec<Session>> {
    let Some(db) = opencode_db() else {
        return Ok(vec![]);
    };
    let condition = match cwd_filter {
        Some(cwd) => format!(
            "s.directory = '{}'",
            cwd.to_string_lossy().replace('\'', "''")
        ),
        None => "s.time_archived IS NULL AND (s.parent_id IS NULL OR s.parent_id = '')".into(),
    };
    let mut out = Vec::new();
    // v1 generation: message roles live in `message.data`, parts aside.
    out.extend(opencode_scan_generation(
        &db,
        &format!("SELECT s.id, s.slug, s.title, s.directory, s.time_created AS created,
        MAX(s.time_updated, COALESCE(MAX(m.time_updated), 0)) AS updated,
        COALESCE(SUM(json_extract(m.data,'$.role') IN ('user','assistant','system')),0) AS messages,
        COALESCE(SUM(json_extract(m.data,'$.role')='user'),0) AS user_turns,
        COALESCE(SUM(json_extract(m.data,'$.role')='assistant'),0) AS asst_turns,
        COALESCE(SUM(json_extract(m.data,'$.role')='assistant' AND json_extract(m.data,'$.error') IS NULL),0) AS ok_asst_turns,
        NULL AS xses_src
        FROM session s LEFT JOIN message m ON m.session_id=s.id
        WHERE {condition} GROUP BY s.id;"),
    )?);
    // v2 generation: message types live in a `type` column with inline
    // content; provenance rides in `session.metadata.xses` (the server
    // regenerates slugs on import, so the v1 slug marker cannot survive).
    if opencode_has_v2(&db) {
        out.extend(opencode_scan_generation(
            &db,
            &format!("SELECT s.id, s.slug, s.title, s.directory, s.time_created AS created,
            MAX(s.time_updated, COALESCE(MAX(m.time_updated), 0)) AS updated,
            COALESCE(SUM(m.type IN ('user','assistant','system')),0) AS messages,
            COALESCE(SUM(m.type='user'),0) AS user_turns,
            COALESCE(SUM(m.type='assistant'),0) AS asst_turns,
            COALESCE(SUM(m.type='assistant' AND m.data NOT LIKE '%\"status\":\"error\"%'),0) AS ok_asst_turns,
            json_extract(s.metadata,'$.xses.srcId') AS xses_src
            FROM session_v2 s LEFT JOIN session_message m ON m.session_id=s.id
            WHERE {condition} GROUP BY s.id;"),
        )?);
    }
    Ok(out)
}

/// True when the store has the v2 generation (opencode ≥ 2.x); older stores
/// carry v1 tables only.
pub(crate) fn opencode_has_v2(db: &std::path::Path) -> bool {
    crate::proc::sqlite_text(
        db,
        "SELECT 1 FROM sqlite_master WHERE name='session_v2';",
        std::time::Duration::from_secs(30),
        "opencode generation probe",
    )
    .is_ok_and(|out| out.contains('1'))
}

/// One generation's session rows → [`Session`]s. Runs one aggregate query
/// instead of four correlated scans.
fn opencode_scan_generation(db: &std::path::Path, sql: &str) -> Result<Vec<Session>> {
    let rows = crate::proc::sqlite_json(
        db,
        sql,
        std::time::Duration::from_secs(60),
        "OpenCode session scan output",
    )?;
    let mut out = Vec::new();
    for obj in rows {
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
        let created =
            chrono::DateTime::<Utc>::from_timestamp_millis(created_ms).unwrap_or_default();
        let updated_ms = obj
            .get("updated")
            .and_then(|v| v.as_i64())
            .unwrap_or(created_ms);
        let mtime = chrono::DateTime::<Utc>::from_timestamp_millis(updated_ms).unwrap_or(created);
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
            created,
            size: 0,
            revision: None,
            messages,
            summary: display_title(Tool::Opencode, &title),
            path: PathBuf::from(format!("opencode://{id}")),
            // Titles are user-editable and can legitimately begin with an
            // agent-looking prefix. The v1 slug is written by xses and is
            // the durable provenance marker there; v2 imports carry
            // `metadata.xses` instead (the server regenerates slugs).
            is_import: slug.starts_with("xses-")
                || slug.starts_with("sessionizer-")
                || obj
                    .get("xses_src")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| !s.is_empty()),
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
        assert_eq!(marker_tool(&marker, "/xses/source"), Some(Tool::Claude));
        assert_eq!(
            display_title(
                marker_tool(&marker, "/xses/source").unwrap_or(Tool::Pi),
                "fix the parser"
            ),
            "[claude] fix the parser"
        );
    }
}
