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
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
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
        // Both store generations: v1 (`session`/`message`/`part`) always,
        // v2 (`session_v2`/`session_message`) when the store has it.
        let v2 = if crate::scan::opencode_has_v2(&db) {
            format!(
                " DELETE FROM session_message WHERE session_id IN ({quoted}); \
                 DELETE FROM session_v2 WHERE id IN ({quoted});"
            )
        } else {
            String::new()
        };
        let sql = format!(
            "PRAGMA foreign_keys=ON; BEGIN IMMEDIATE; \
             DELETE FROM part WHERE session_id IN ({quoted}); \
             DELETE FROM message WHERE session_id IN ({quoted}); \
             DELETE FROM session WHERE id IN ({quoted});{v2} COMMIT;"
        );
        // Generous per chunk: 128 sessions of imports measure at ~20-30s.
        if let Err(e) =
            crate::proc::sqlite_exec(&db, &sql, Duration::from_secs(300), "OpenCode bulk cleanup")
        {
            anyhow::bail!("OpenCode bulk cleanup failed after {removed}/{total}: {e}");
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
    snapshots: &mut std::collections::HashMap<String, crate::extract::opencode::Snapshot>,
) -> Result<Vec<(&'a Session, Result<CanonicalSession>)>> {
    // Snapshots read in the refresh step are single-use: move them into the
    // batch inputs here so a retry re-reads rather than replaying stale rows.
    let inputs: Vec<(&Session, Option<crate::canonical::Events>)> = sources
        .iter()
        .map(|&source| {
            let snapshot = (source.tool == Tool::Opencode)
                .then(|| snapshots.remove(&source.id))
                .flatten()
                .map(|snap| snap.events);
            (source, snapshot)
        })
        .collect();
    std::thread::scope(|threads| {
        let workers: Vec<_> = inputs
            .into_iter()
            .map(|(source, snapshot)| {
                threads.spawn(move || {
                    let canon = match snapshot {
                        Some(events) => Ok(crate::extract::canonicalize_events(source, events)),
                        None => crate::extract::canonicalize(source),
                    }
                    .map(|c| match scope {
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
    let targets: Vec<Tool> = if targets.is_empty() {
        Tool::all_tools().to_vec()
    } else {
        targets.to_vec()
    };
    let mut index = Index::load()?;
    if !dry_run {
        index.commit(Vec::new())?;
    }
    let mut report = SyncReport {
        targets: targets
            .iter()
            .map(|&target| TargetReport::new(target))
            .collect(),
    };
    let mut sources: Vec<_> = sessions
        .iter()
        .filter(|s| include_imports || !s.is_import)
        .cloned()
        .collect();
    let mut snapshots = refresh_opencode_revisions(&mut sources, &targets)?;
    // Pre-snapshot keys migrate without re-importing (below); merged into
    // `seen` either way so dry runs report what a real run would skip.
    let migrated = migrate_opencode_keys(&index, &sources, &targets, scope)?;
    let migrated_keys: HashSet<String> = migrated.iter().map(|r| r.key.clone()).collect();
    if !dry_run && !migrated.is_empty() {
        index.commit(migrated)?;
    }
    let mut seen = index.keys();
    seen.extend(migrated_keys);
    let pending = plan_pending(&sources, &mut report, &seen, scope, dry_run, progress);
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
        sync_batch(
            &pending[start..end],
            &mut report,
            &seen,
            scope,
            dry_run,
            progress,
            &mut index,
            &mut snapshots,
        )?;
        start = end;
    }
    Ok(report)
}

/// OpenCode's own content digest also catches edits/deletions where an
/// upstream client forgot to advance session.time_updated. Snapshots run in
/// parallel and carry the parsed transcript with the digest, so the extract
/// batch below reuses the same read instead of querying twice.
fn refresh_opencode_revisions(
    sources: &mut [Session],
    targets: &[Tool],
) -> Result<HashMap<String, crate::extract::opencode::Snapshot>> {
    let indexed: Vec<(usize, &str)> = sources
        .iter()
        .enumerate()
        .filter(|(_, s)| s.tool == Tool::Opencode && targets.iter().any(|t| *t != s.tool))
        .map(|(i, s)| (i, s.id.as_str()))
        .collect();
    let snaps: Vec<(usize, crate::extract::opencode::Snapshot)> =
        std::thread::scope(|threads| -> Result<Vec<_>> {
            let workers: Vec<_> = indexed
                .into_iter()
                .map(|(i, id)| {
                    threads.spawn(move || {
                        crate::extract::opencode::opencode_snapshot(id).map(|snap| (i, snap))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| {
                    w.join()
                        .map_err(|_| anyhow::anyhow!("revision worker panicked"))?
                })
                .collect()
        })?;
    let mut snapshots = HashMap::new();
    for (i, snap) in snaps {
        sources[i].revision = Some(snap.revision.clone());
        sources[i].size = snap.size;
        snapshots.insert(sources[i].id.clone(), snap);
    }
    Ok(snapshots)
}

/// Migrate pre-snapshot digest keys without re-importing: a new-key miss
/// that hits the legacy key means "already synced" — the new key is recorded
/// over the old artifacts, so the next sync hits directly. Legacy queries run
/// only on new-key misses and taper to zero as sources migrate; a legacy
/// failure just means "not migrated", i.e. a genuine re-sync.
/// Returns index records for the migrated keys; the caller commits them
/// (honoring dry-run) and merges the keys into its seen set.
fn migrate_opencode_keys(
    index: &Index,
    sources: &[Session],
    targets: &[Tool],
    scope: Option<usize>,
) -> Result<Vec<Record>> {
    let seen = index.keys();
    let mut updates = Vec::new();
    for source in sources {
        if source.tool != Tool::Opencode {
            continue;
        }
        for target in targets {
            if source.tool == *target {
                continue;
            }
            let new_key = fingerprint(*target, source, scope);
            if seen.contains(&new_key) || updates.iter().any(|r: &Record| r.key == new_key) {
                continue;
            }
            let Ok(legacy_rev) = crate::extract::opencode::legacy_revision(&source.id) else {
                continue;
            };
            let mut legacy_source = source.clone();
            legacy_source.revision = Some(legacy_rev);
            let legacy_key = fingerprint(*target, &legacy_source, scope);
            if !seen.contains(&legacy_key) {
                continue;
            }
            if let Some(old) = index.records().iter().find(|r| r.key == legacy_key) {
                updates.push(Record {
                    key: new_key,
                    paths: old.paths.clone(),
                });
            }
        }
    }
    Ok(updates)
}

/// Split sources into already-synced (counted as skipped) and still pending,
/// registering progress for the latter.
fn plan_pending<'a>(
    sources: &'a [Session],
    report: &mut SyncReport,
    seen: &HashSet<String>,
    scope: Option<usize>,
    dry_run: bool,
    progress: &crate::progress::Progress,
) -> Vec<&'a Session> {
    let mut pending = Vec::new();
    for source in sources {
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
    pending
}

/// One batch: extract transcripts, fan out per target, merge the reports.
#[allow(clippy::too_many_arguments)]
fn sync_batch(
    batch: &[&Session],
    report: &mut SyncReport,
    seen: &HashSet<String>,
    scope: Option<usize>,
    dry_run: bool,
    progress: &crate::progress::Progress,
    index: &mut Index,
    snapshots: &mut HashMap<String, crate::extract::opencode::Snapshot>,
) -> Result<()> {
    let extracted = canonicalize_batch(batch, scope, snapshots)?;
    let targets: Vec<Tool> = report.targets.iter().map(|t| t.target).collect();
    let completed = std::thread::scope(|threads| -> Result<Vec<_>> {
        let workers: Vec<_> = targets
            .iter()
            .map(|&target| {
                let extracted = &extracted;
                threads
                    .spawn(move || write_target(target, extracted, seen, scope, dry_run, progress))
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
    Ok(())
}

/// One target's share of a batch: skip synced/empty rows, write the rest.
fn write_target(
    target: Tool,
    extracted: &[(&Session, Result<CanonicalSession>)],
    seen: &HashSet<String>,
    scope: Option<usize>,
    dry_run: bool,
    progress: &crate::progress::Progress,
) -> (TargetReport, Vec<Record>) {
    let mut updates = Vec::new();
    let mut result = TargetReport::new(target);
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
}

impl TargetReport {
    fn new(target: Tool) -> Self {
        Self {
            target,
            written: 0,
            skipped: 0,
            failed: Vec::new(),
            loss: LossReport::default(),
        }
    }
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

    /// Needs a live opencode store; CI machines skip it. Exercises the
    /// whole migration: legacy-key record in, new-key record over the same
    /// artifacts out, no re-import needed.
    /// Fabricates a v1-only opencode store (a session with one part, so the
    /// two digest shapes differ) and exercises the whole migration:
    /// legacy-key record in, new-key record over the same artifacts out.
    /// Skips when `sqlite3` is unavailable.
    #[test]
    fn legacy_digest_keys_migrate_without_reimport() {
        if std::process::Command::new("sqlite3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("xses-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let sql = "CREATE TABLE session(id TEXT PRIMARY KEY, title TEXT, directory TEXT, \
            time_created INTEGER); \
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT); \
            CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
            time_created INTEGER, data TEXT); \
            INSERT INTO session VALUES('ses_probe1', 't', '/tmp', 1000); \
            INSERT INTO message VALUES('m1', 'ses_probe1', 1001, '{\"role\":\"user\"}'); \
            INSERT INTO part VALUES('p1', 'm1', 'ses_probe1', 1002, '{\"type\":\"text\",\"text\":\"hi\"}');";
        let status = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg(sql)
            .status()
            .expect("run sqlite3");
        assert!(status.success());
        let prev = std::env::var_os("OPENCODE_DATA_DIR");
        std::env::set_var("OPENCODE_DATA_DIR", &dir);
        let result = legacy_migration_against(&dir);
        match prev {
            Some(v) => std::env::set_var("OPENCODE_DATA_DIR", v),
            None => std::env::remove_var("OPENCODE_DATA_DIR"),
        }
        let (new_key, legacy_key, updates, artifact) = result.expect("migrate");
        // The part row's `message_id` separates the two digest shapes.
        assert_ne!(new_key, legacy_key);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].key, new_key);
        assert_eq!(updates[0].paths, vec![artifact]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn legacy_migration_against(
        dir: &std::path::Path,
    ) -> anyhow::Result<(
        String,
        String,
        Vec<crate::index::Record>,
        std::path::PathBuf,
    )> {
        let id = "ses_probe1";
        let new_rev = crate::extract::opencode::opencode_snapshot(id)?.revision;
        let legacy_rev = crate::extract::opencode::legacy_revision(id)?;
        let now = chrono::Utc::now();
        let source = crate::model::Session {
            id: id.to_string(),
            tool: Tool::Opencode,
            cwd: std::path::PathBuf::from("/tmp"),
            mtime: now,
            created: now,
            size: 7,
            revision: Some(new_rev),
            messages: 1,
            summary: String::new(),
            path: std::path::PathBuf::from("opencode://x"),
            is_import: false,
            noise: None,
        };
        let target = Tool::Codex;
        let new_key = fingerprint(target, &source, None);
        let mut legacy_source = source.clone();
        legacy_source.revision = Some(legacy_rev);
        let legacy_key = fingerprint(target, &legacy_source, None);
        let artifact = dir.join("rollout.jsonl");
        std::fs::write(&artifact, b"x")?;
        // The in-memory index only carries the legacy key, as recorded
        // before the snapshot change.
        let index = Index {
            path: dir.join("index.jsonl"),
            records: vec![crate::index::Record {
                key: legacy_key.clone(),
                paths: vec![artifact.clone()],
            }],
        };
        let updates = migrate_opencode_keys(&index, &[source], &[target], None)?;
        Ok((new_key, legacy_key, updates, artifact))
    }
}
