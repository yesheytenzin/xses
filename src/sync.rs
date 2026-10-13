//! Sync orchestration: import foreign sessions into target stores via the IR.
//!
//! Rules:
//! - Sources are always read-only.
//! - Files xses itself wrote carry an import marker and are skipped as
//!   sources unless `include_imports` is set (echo protection).
//! - Extraction runs once per source (parallel), writers fan out per target.
//! - Idempotent via `~/.local/share/xses/index.jsonl`.
//! - CLI-backed targets (opencode) are imported through their own CLI, never
//!   by writing their database directly.

use crate::canonical::{CanonicalSession, LossReport};
use crate::index::{Index, Record};
use crate::model::{Session, Tool};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Sessions deleted per sqlite3 invocation. Import rows are blob-heavy and
/// free at roughly 3k rows/s on a store with hundreds of imports, so one
/// monolithic transaction outlives any sane deadline; chunks keep each
/// command bounded and make progress visible.
const OPENCODE_DELETE_CHUNK: usize = 128;

fn delete_opencode_imports(
    ids: &HashSet<String>,
    progress: &crate::progress::Progress,
) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let Some(db) = crate::scan::opencode_db() else {
        return Ok(0);
    };
    let total = ids.len();
    let mut removed = 0usize;
    let ids: Vec<&String> = ids.iter().collect();
    for chunk in ids.chunks(OPENCODE_DELETE_CHUNK) {
        let quoted = chunk
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "PRAGMA foreign_keys=ON; BEGIN IMMEDIATE; \
             DELETE FROM part WHERE session_id IN ({quoted}); \
             DELETE FROM message WHERE session_id IN ({quoted}); \
             DELETE FROM session WHERE id IN ({quoted}); COMMIT;"
        );
        let mut command = Command::new("sqlite3");
        command.args(["-cmd", ".timeout 5000"]).arg(&db).arg(sql);
        // Generous per chunk: 128 sessions of imports measure at ~20-30s.
        let output = crate::proc::output_with_timeout(command, Duration::from_secs(300))?;
        if !output.status.success() {
            anyhow::bail!(
                "OpenCode bulk cleanup failed after {removed}/{total}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        removed += chunk.len();
        progress.inc(&format!("opencode {removed}/{total} sessions"));
    }
    Ok(removed)
}

fn acquire_index_lock() -> Result<crate::storage::Lock> {
    let path = crate::index::path()?.with_extension("lock");
    crate::storage::Lock::try_acquire(&path)?.with_context(|| {
        format!(
            "another xses sync or clean is still running (lock: {})",
            path.display()
        )
    })
}

/// Idempotency key: target + source tool + source id + mtime + size, plus the
/// writer mapping version — a mapping change must re-import so older, poorer
/// copies get upgraded instead of lingering.
pub fn fingerprint(target: Tool, src: &Session, scope: Option<usize>) -> String {
    format!(
        "{}|{}|{}|{}|{}|m{}|scope:{}|revision:{}",
        target.as_str(),
        src.tool.as_str(),
        src.id,
        src.mtime.timestamp_nanos_opt().unwrap_or(0),
        src.size,
        crate::write::MAPPING,
        scope.map(|n| n.to_string()).unwrap_or_else(|| "all".into()),
        src.revision.as_deref().unwrap_or("file"),
    )
}

/// Remove every xses-written import (index-recorded + swept by name and
/// content markers), plus their codex `threads`/projection rows and opencode
/// sessions. Truncates the index — keys can't be mapped back reliably after
/// mass removal. Returns the number of artifacts removed.
pub fn clean(dry_run: bool, progress: &crate::progress::Progress) -> Result<usize> {
    progress.add_total(1);
    clean_impl(dry_run, None, progress)
}

/// Remove only the imports of the given source sessions (their artifacts in
/// every target). Keeps the rest of the index intact.
pub fn clean_selected(
    dry_run: bool,
    selected: &HashSet<(Tool, String)>,
    progress: &crate::progress::Progress,
) -> Result<usize> {
    if selected.is_empty() {
        return Ok(0);
    }
    progress.add_total(1);
    clean_impl(dry_run, Some(selected), progress)
}

/// Import artifact ids belonging to the selection; `None` matches everything.
/// Import filenames and opencode slugs embed this deterministic id.
fn selection_matcher(only: Option<&HashSet<(Tool, String)>>) -> impl Fn(&str) -> bool + '_ {
    move |name: &str| match only {
        None => true,
        Some(only) => only
            .iter()
            .any(|(tool, id)| name.contains(&crate::write::import_id_for(*tool, id))),
    }
}

fn clean_impl(
    dry_run: bool,
    only: Option<&HashSet<(Tool, String)>>,
    progress: &crate::progress::Progress,
) -> Result<usize> {
    let _lock = acquire_index_lock()?;
    let mut index = Index::load()?;
    let mut paths: HashSet<PathBuf> = index
        .records()
        .iter()
        .filter(|r| {
            only.is_none_or(|selected| {
                crate::index::source_of(&r.key).is_some_and(|src| selected.contains(&src))
            })
        })
        .flat_map(|r| r.paths.iter().cloned())
        .collect();
    let home = dirs::home_dir().context("no home dir")?;
    for base in [
        ".claude/projects",
        ".pi/agent/sessions",
        ".omp/agent/sessions",
        ".codex/sessions",
    ] {
        let root = home.join(base);
        if !root.is_dir() {
            continue;
        }
        // Collect before deleting so sidecars can still consult their rollout.
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy();
            let marker_path = if name == "custom-title.json" {
                let Some(dir) = path.parent() else {
                    continue;
                };
                dir.with_extension("jsonl")
            } else {
                path.to_owned()
            };
            let imported = is_import_filename(&name)
                || (name.ends_with(".bak")
                    && (is_import_stem(&name) || first_line_has_marker(path)))
                || (name.ends_with(".jsonl") && first_line_has_marker(path))
                || (name == "custom-title.json" && first_line_has_marker(&marker_path));
            if !imported {
                continue;
            }
            let selected = only.is_none_or(|selected| {
                import_provenance(&marker_path).is_some_and(|source| selected.contains(&source))
                    || selection_matcher(Some(selected))(&name)
            });
            if selected {
                paths.insert(path.into());
            }
        }
    }
    for id in crate::scan::opencode_import_ids(only)? {
        paths.insert(PathBuf::from(format!("opencode://{id}")));
    }
    let paths: Vec<_> = paths.into_iter().collect();
    progress.add_total(paths.len());
    if !dry_run {
        retire_artifacts(&paths)?;
        index.remove_sources(only)?;
    }
    Ok(paths.len())
}

fn import_provenance(path: &std::path::Path) -> Option<(Tool, String)> {
    let head = crate::jsonl::head_bytes(path, 64 * 1024)?;
    let row: serde_json::Value = serde_json::from_str(head.lines().next()?).ok()?;
    let marker = row
        .get("xses")
        .or_else(|| row.get("sessionizer"))
        .or_else(|| row.pointer("/payload/xses"))
        .or_else(|| row.pointer("/payload/sessionizer"))?;
    Some((
        Tool::parse(marker.get("source")?.as_str()?)?,
        marker.get("srcId")?.as_str()?.to_owned(),
    ))
}

pub struct TargetReport {
    pub target: Tool,
    pub written: usize,
    pub skipped: usize,
    pub failed: Vec<String>,
    pub loss: LossReport,
}

pub struct SyncReport {
    pub targets: Vec<TargetReport>,
}

impl SyncReport {
    pub fn totals(&self) -> (usize, usize, LossReport) {
        let mut written = 0;
        let mut skipped = 0;
        let mut loss = LossReport::default();
        for t in &self.targets {
            written += t.written;
            skipped += t.skipped;
            loss.merge(t.loss);
        }
        (written, skipped, loss)
    }
}

/// Extract a bounded batch. Returning the source beside its result avoids
/// repeated linear lookups, and scoped history is computed just once.
fn canonicalize_batch<'a>(
    sources: &[&'a Session],
    scope: Option<usize>,
) -> Result<Vec<(&'a Session, Result<CanonicalSession>)>> {
    std::thread::scope(|threads| {
        let workers: Vec<_> = sources
            .iter()
            .map(|&source| {
                threads.spawn(move || {
                    let canon = crate::extract::canonicalize(source).map(|c| match scope {
                        Some(n) => crate::scope::last_n(c, n),
                        None => c,
                    });
                    (source, canon)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .map_err(|_| anyhow::anyhow!("transcript worker panicked"))
            })
            .collect()
    })
}

/// At most four transcripts / approximately 32 MiB of source bytes per batch.
/// A single large source is processed alone; memory never grows with the
/// total number of selected sessions.
fn batch_end(sources: &[&Session], start: usize) -> usize {
    let mut end = start;
    let mut bytes = 0u64;
    while end < sources.len() && end - start < 4 {
        let next = sources[end].size;
        if end > start && bytes.saturating_add(next) > 32 * 1024 * 1024 {
            break;
        }
        bytes = bytes.saturating_add(next);
        end += 1;
    }
    end
}

pub fn sync(
    targets: &[Tool],
    sessions: &[Session],
    dry_run: bool,
    include_imports: bool,
    scope: Option<usize>,
    progress: &crate::progress::Progress,
) -> Result<SyncReport> {
    let _lock = acquire_index_lock()?;
    let targets = if targets.is_empty() {
        Tool::all_tools()
    } else {
        targets
    };
    let mut index = Index::load()?;
    if !dry_run {
        index.commit(Vec::new())?;
    }
    let seen = index.keys();
    let mut report = SyncReport {
        targets: targets
            .iter()
            .map(|&target| TargetReport {
                target,
                written: 0,
                skipped: 0,
                failed: Vec::new(),
                loss: LossReport::default(),
            })
            .collect(),
    };
    // OpenCode's own content digest also catches edits/deletions where an
    // upstream client forgot to advance session.time_updated.
    let mut sources: Vec<_> = sessions
        .iter()
        .filter(|s| include_imports || !s.is_import)
        .cloned()
        .collect();
    for source in &mut sources {
        if source.tool == Tool::Opencode && targets.iter().any(|t| *t != source.tool) {
            let (revision, size) = crate::extract::opencode_revision(&source.id)?;
            source.revision = Some(revision);
            source.size = size;
        }
    }
    let mut pending = Vec::new();
    for source in &sources {
        let mut needed = false;
        for target in &mut report.targets {
            if source.tool == target.target {
                continue;
            }
            if seen.contains(&fingerprint(target.target, source, scope)) {
                target.skipped += 1;
            } else {
                needed = true;
                if !dry_run {
                    progress.add_total(1);
                }
            }
        }
        if needed {
            pending.push(source);
        }
    }
    progress.flush(&format!("{} transcripts need importing", pending.len()));
    let mut start = 0;
    while start < pending.len() {
        let end = batch_end(&pending, start);
        progress.status(&format!(
            "reading transcripts {}–{} of {}",
            start + 1,
            end,
            pending.len()
        ));
        let extracted = canonicalize_batch(&pending[start..end], scope)?;
        let completed = std::thread::scope(|threads| -> Result<Vec<_>> {
            let workers: Vec<_> = targets
                .iter()
                .map(|&target| {
                    let extracted = &extracted;
                    let seen = &seen;
                    threads.spawn(move || {
                        let mut updates = Vec::new();
                        let mut result = TargetReport {
                            target,
                            written: 0,
                            skipped: 0,
                            failed: Vec::new(),
                            loss: LossReport::default(),
                        };
                        for (source, canonical) in extracted {
                            if source.tool == target {
                                continue;
                            }
                            let key = fingerprint(target, source, scope);
                            if seen.contains(&key) {
                                continue;
                            }
                            let canonical = match canonical {
                                Ok(c) => c,
                                Err(e) => {
                                    result.failed.push(format!("{}: {e}", source.short()));
                                    continue;
                                }
                            };
                            let (has_content, loss) = canonical.transfer_summary();
                            if !has_content {
                                result.skipped += 1;
                                continue;
                            }
                            if dry_run {
                                result.written += 1;
                                result.loss.merge(loss);
                                continue;
                            }
                            // A new generation cannot overwrite the last working import.
                            let outcome = crate::write::session_revision(target, canonical, &key);
                            match outcome {
                                Ok((artifacts, loss)) => {
                                    updates.push(Record {
                                        key,
                                        paths: artifacts,
                                    });
                                    result.written += 1;
                                    result.loss.merge(loss);
                                    progress.inc(&format!("{} {}", target.as_str(), source.id));
                                }
                                Err(e) => {
                                    let detail = format!("{}: {e}", source.short());
                                    crate::issues::record("import-failed", &source.id, &detail);
                                    result.failed.push(detail);
                                    progress.fail(&format!("{} {}", target.as_str(), source.id));
                                }
                            }
                        }
                        (result, updates)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| {
                    w.join()
                        .map_err(|_| anyhow::anyhow!("sync worker panicked"))
                })
                .collect()
        })?;
        let mut updates = Vec::new();
        for (result, records) in completed {
            let total = report
                .targets
                .iter_mut()
                .find(|t| t.target == result.target)
                .expect("known target");
            total.written += result.written;
            total.skipped += result.skipped;
            total.failed.extend(result.failed);
            total.loss.merge(result.loss);
            updates.extend(records);
        }
        if !dry_run {
            index.commit(updates)?;
        }
        start = end;
    }
    Ok(report)
}

/// Retire only paths belonging to obsolete generations. Missing files are
/// already retired; any other error keeps the record for a later retry.
pub(crate) fn retire_artifacts(paths: &[PathBuf]) -> Result<()> {
    let mut opencode = HashSet::new();
    for path in paths {
        if let Some(id) = path.to_string_lossy().strip_prefix("opencode://") {
            opencode.insert(id.to_owned());
            continue;
        }
        if path.extension().and_then(|p| p.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|p| p.to_str())
                .is_some_and(|s| s.starts_with("rollout-"))
        {
            crate::write::retire_codex(path)?;
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("retire {}", path.display())),
        }
    }
    delete_opencode_imports(&opencode, &crate::progress::Progress::disabled())?;
    Ok(())
}

/// True for xses-written filenames, which are structurally distinct from
/// every native store's naming.
fn is_import_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let stem = stem.strip_prefix("rollout-").unwrap_or(stem);
    // import: `<iso-stamp>_<8 or 32 alnum>`; native: `<ts>-<uuid>`
    let Some((ts, id)) = stem.split_once('_') else {
        return false;
    };
    if id.contains('-')
        || !id.chars().all(|c| c.is_ascii_alphanumeric())
        || !matches!(id.len(), 8 | 32)
    {
        return false;
    }
    // ts looks like `2026-10-11T17-18-05` (imports) not `2026-10-11T13-02-53-<uuid>`.
    // Native pi/codex separate ts and uuid with `-`; ours uses `_`.
    ts.starts_with("20") && ts.len() == 19 && ts.contains('T')
}

/// Import-name check ignoring extension (for `.bak` residue detection).
fn is_import_stem(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    is_import_filename(&format!("{stem}.jsonl"))
}

/// True if the file's first line carries an xses or legacy provenance marker.
fn first_line_has_marker(path: &std::path::Path) -> bool {
    let Some(head) = crate::jsonl::head_bytes(path, 2000) else {
        return false;
    };
    head.contains("\"xses\":")
        || head.contains("\"sessionizer\":")
        || head.contains("\"cli_version\":\"0.0.0-xses\"")
        || head.contains("\"cli_version\":\"0.0.0-sessionizer\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::decode_paths;

    #[test]
    fn index_paths_support_commas_without_breaking_legacy_lines() {
        assert_eq!(
            decode_paths("[\"/tmp/a,b\",\"/tmp/c\"]"),
            vec!["/tmp/a,b", "/tmp/c"]
        );
        assert_eq!(decode_paths("/tmp/a,/tmp/b"), vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn import_filenames_accept_old_and_new_ids() {
        assert!(is_import_filename("2026-10-12T10-20-30_abcdef12.jsonl"));
        assert!(is_import_filename(
            "2026-10-12T10-20-30_0123456789abcdef0123456789abcdef.jsonl"
        ));
        assert!(!is_import_filename("2026-10-12T10-20-30-native.jsonl"));
    }
}
