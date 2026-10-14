//! Counts avoid materializing OpenCode's full session listing. Database/WAL
//! stamps cache stable results without writing anything into native stores.
use crate::model::Tool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Counts {
    pub total: usize,
    pub empty: usize,
    pub unanswered: usize,
    pub failed: usize,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    version: u32,
    database: PathBuf,
    stamp: Vec<Option<(u64, u64)>>,
    counts: Counts,
}

pub(super) fn stamp(db: &Path) -> Result<Vec<Option<(u64, u64)>>> {
    [
        db.to_path_buf(),
        PathBuf::from(format!("{}-wal", db.display())),
        PathBuf::from(format!("{}-journal", db.display())),
    ]
    .iter()
    .map(|path| match std::fs::metadata(path) {
        Ok(md) => Ok(Some((
            md.len(),
            md.modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    })
    .collect()
}

fn opencode() -> Result<Counts> {
    let Some(db) = super::opencode_db() else {
        return Ok(Counts::default());
    };
    let path = crate::index::path()?.with_file_name("opencode-count-cache.json");
    let before = stamp(&db)?;
    if let Some(cached) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cached>(&bytes).ok())
    {
        if cached.version == 1 && cached.database == db && cached.stamp == before {
            return Ok(cached.counts);
        }
    }
    let (has_v1, has_v2) = super::opencode_generations(&db)?;
    let mut legs = Vec::new();
    if has_v1 {
        let exclude = if has_v2 {
            " AND NOT EXISTS(SELECT 1 FROM session_v2 newer WHERE newer.id=s.id)"
        } else {
            ""
        };
        legs.push(format!("SELECT s.id, COALESCE(SUM(json_extract(m.data,'$.role')='user'),0) AS users, COALESCE(SUM(json_extract(m.data,'$.role')='assistant'),0) AS assistants, COALESCE(SUM(json_extract(m.data,'$.role')='assistant' AND json_extract(m.data,'$.error') IS NULL),0) AS ok FROM session s LEFT JOIN message m ON m.session_id=s.id WHERE s.time_archived IS NULL AND (s.parent_id IS NULL OR s.parent_id=''){exclude} GROUP BY s.id"));
    }
    if has_v2 {
        legs.push("SELECT s.id, COALESCE(SUM(m.type='user'),0) AS users, COALESCE(SUM(m.type='assistant'),0) AS assistants, COALESCE(SUM(m.type='assistant' AND m.data NOT LIKE '%\"status\":\"error\"%'),0) AS ok FROM session_v2 s LEFT JOIN session_message m ON m.session_id=s.id WHERE s.time_archived IS NULL AND (s.parent_id IS NULL OR s.parent_id='') GROUP BY s.id".to_owned());
    }
    let stats = legs.join(" UNION ALL ");
    let sql = format!(
        "WITH stats AS ({stats}) SELECT COUNT(*) AS total, \
        COALESCE(SUM(users=0 AND assistants=0),0) AS empty, \
        COALESCE(SUM(users>0 AND assistants=0),0) AS unanswered, \
        COALESCE(SUM(assistants>0 AND ok=0),0) AS failed FROM stats;"
    );
    let rows = crate::proc::sqlite_json(
        &db,
        &sql,
        std::time::Duration::from_secs(60),
        "OpenCode counts",
    )?;
    let counts = rows
        .first()
        .map(|row| serde_json::from_value::<Counts>(row.clone()))
        .transpose()?
        .unwrap_or_default();
    if before == stamp(&db)? {
        let cached = Cached {
            version: 1,
            database: db,
            stamp: before,
            counts,
        };
        let lock = crate::storage::Lock::try_acquire(&path.with_extension("lock"))
            .ok()
            .flatten();
        if lock.is_some() {
            let _ = crate::storage::atomic_write(&path, &serde_json::to_vec(&cached)?);
        }
        return Ok(cached.counts);
    }
    Ok(counts)
}

pub(crate) fn all() -> Result<Vec<(Tool, Counts)>> {
    std::thread::scope(|threads| {
        let workers: Vec<_> = Tool::all_tools()
            .iter()
            .map(|&tool| {
                threads.spawn(move || -> Result<_> {
                    if tool == Tool::Opencode {
                        return Ok((tool, opencode()?));
                    }
                    let mut sessions = super::files::scan(tool, None, true)?;
                    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.cmp(&b.id)));
                    let mut seen = std::collections::HashSet::new();
                    sessions.retain(|session| seen.insert(session.id.clone()));
                    let mut counts = Counts {
                        total: sessions.len(),
                        ..Counts::default()
                    };
                    for session in sessions {
                        match session.noise.as_deref() {
                            Some("empty") => counts.empty += 1,
                            Some("unanswered") => counts.unanswered += 1,
                            Some("failed") => counts.failed += 1,
                            _ => {}
                        }
                    }
                    Ok((tool, counts))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("count worker panicked"))?
            })
            .collect()
    })
}
