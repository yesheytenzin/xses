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
use crate::build;
use crate::canonical::{CanonicalSession, LossReport};
use crate::model::{Session, Tool};
use crate::refs;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn command_output_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command exceeded {} seconds", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn delete_opencode_imports(ids: &HashSet<String>) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let Some(db) = crate::scan::opencode_db() else {
        return Ok(0);
    };
    let quoted = ids
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
    command.args(["-cmd", ".timeout 5000"]).arg(db).arg(sql);
    let output = command_output_timeout(command, Duration::from_secs(10))?;
    if !output.status.success() {
        anyhow::bail!(
            "OpenCode bulk cleanup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(ids.len())
}

fn index_path() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no data dir")?;
    Ok(base.join("xses/index.jsonl"))
}

struct IndexLock {
    path: PathBuf,
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_index_lock() -> Result<IndexLock> {
    use std::io::Write;
    let index = index_path()?;
    let path = index.with_extension("lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    for _ in 0..100 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                return Ok(IndexLock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .map(|pid| {
                        pid != std::process::id()
                            && !std::path::Path::new(&format!("/proc/{pid}")).exists()
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "another xses sync or clean is still running (lock: {})",
        path.display()
    )
}

/// Idempotency key: target + source tool + source id + mtime + size.
pub fn fingerprint(target: Tool, src: &Session) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        target.as_str(),
        src.tool.as_str(),
        src.id,
        src.mtime.timestamp_nanos_opt().unwrap_or(0),
        src.size
    )
}

fn load_index() -> Result<HashSet<String>> {
    let p = index_path()?;
    if !p.exists() {
        return Ok(HashSet::new());
    }
    let mut set = HashSet::new();
    for line in BufReader::new(fs::File::open(&p)?).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            if let Some(k) = line.split('\t').next() {
                set.insert(k.to_string());
            }
        }
    }
    Ok(set)
}

/// Record an idempotency key plus the concrete paths it produced, so `clean`
/// can undo exactly what sync wrote (marked or not).
pub fn remember(key: &str, files: Vec<PathBuf>) -> Result<()> {
    use std::io::Write;
    let p = index_path()?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(p)?;
    let paths: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    writeln!(f, "{key}\t{}", serde_json::to_string(&paths)?)?;
    f.sync_data()?;
    Ok(())
}

fn decode_paths(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
        raw.split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// Remove every xses-written import (index-recorded + swept by name and
/// content markers), plus their codex `threads`/projection rows and opencode
/// sessions. Truncates the index — keys can't be mapped back reliably after
/// mass removal. Returns the number of artifacts removed.
pub fn clean(dry_run: bool) -> Result<usize> {
    clean_impl(dry_run)
}

fn clean_impl(dry_run: bool) -> Result<usize> {
    let _lock = acquire_index_lock()?;
    let p = index_path()?;
    let mut removed = 0usize;
    let mut keep = Vec::new();
    if p.exists() {
        for line in BufReader::new(fs::File::open(&p)?).lines() {
            let line = line?;
            let mut parts = line.splitn(2, '\t');
            let _key = parts.next().unwrap_or("");
            let paths = parts.next().unwrap_or("");
            if paths.is_empty() {
                keep.push(line);
                continue;
            }
            let mut line_ok = true;
            for path in decode_paths(paths) {
                let mut removed_this = false;
                if let Some(id) = path.strip_prefix("opencode://") {
                    if !dry_run {
                        // OpenCode imports are deleted in one transaction below;
                        // per-session CLI deletes can block for every duplicate.
                        removed_this = !id.is_empty();
                    }
                } else {
                    let f = PathBuf::from(&path);
                    if f.exists() {
                        removed_this = dry_run || fs::remove_file(&f).is_ok();
                    }
                }
                line_ok &= removed_this;
                if removed_this {
                    removed += 1;
                }
            }
            if line_ok {
                keep.push(line);
            }
        }
    }

    {
        // Imports that predate path recording. Import filenames are structurally
        // distinct from native ones:
        //   claude native `<uuid>.jsonl`         import `<iso>_<8hex>.jsonl`
        //   pi/omp native `<ts>_<ulid>.jsonl`     import `<iso>_<8hex>.jsonl`
        //   codex native `rollout-<ts>-<uuid>`    import `rollout-<iso>_<8hex>`
        let home = dirs::home_dir().context("no home dir")?;
        let mut removed_codex: Vec<PathBuf> = Vec::new();
        let stores = [
            home.join(".claude/projects"),
            home.join(".pi/agent/sessions"),
            home.join(".omp/agent/sessions"),
            home.join(".codex/sessions"),
        ];
        for root in stores {
            if !root.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if !entry.file_type().is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let sweep = is_import_filename(name)
                    || (name.ends_with(".bak")
                        && (is_import_stem(name) || first_line_has_marker(path)))
                    // new-format claude imports are uuid-named like natives;
                    // detect them by the marker in the first line.
                    || (name.ends_with(".jsonl") && first_line_has_marker(path));
                if !sweep {
                    continue;
                }
                let removed_this = dry_run || fs::remove_file(path).is_ok();
                if removed_this && path.starts_with(home.join(".codex/sessions")) {
                    removed_codex.push(path.to_path_buf());
                }
                if removed_this {
                    removed += 1;
                }
            }
        }
        // Drop the picker-index rows and history projections for removed codex rollouts.
        // Codex owns this history database and may hold a kernel-level lock
        // while its UI is running. Do not block clean/sync on projection
        // maintenance; rollout files are the authoritative imported artifacts.
        // The old maintenance code remains opt-in for manual recovery.
        if !removed_codex.is_empty()
            && !dry_run
            && (std::env::var_os("XSES_CLEAN_CODEX_DB").is_some()
                || std::env::var_os("SESSIONIZER_CLEAN_CODEX_DB").is_some())
        {
            if let Some(db) = crate::build::codex_state_db_pub(&home) {
                let list: Vec<String> = removed_codex
                    .iter()
                    .map(|p| format!("'{}'", p.to_string_lossy().replace('\'', "''")))
                    .collect();
                let paths = list.join(",");
                let ids: String = removed_codex
                    .iter()
                    .filter_map(|p| {
                        // rollout-<ts>-<uuid>: the uuid is the thread id
                        let stem = p.file_stem()?.to_str()?;
                        let name = stem.strip_prefix("rollout-")?;
                        let uuid = name
                            .rsplit('-')
                            .take(5)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("-");
                        if uuid.len() == 36 {
                            Some(format!("'{uuid}'"))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "DELETE FROM threads WHERE originator IN ('xses', 'sessionizer') OR rollout_path IN ({paths});"
                );
                let _ = std::process::Command::new("sqlite3")
                    .args(["-cmd", ".timeout 5000"])
                    .arg(&db)
                    .arg(&sql)
                    .output();
                // also drop import rows whose rollout file is already gone
                if let Ok(out) = std::process::Command::new("sqlite3")
                    .args(["-cmd", ".timeout 5000", "-json"])
                    .arg(&db)
                    .arg("SELECT id, rollout_path FROM threads WHERE cli_version IN ('0.0.0-xses', '0.0.0-sessionizer');")
                    .output()
                {
                    if let Ok(rows) =
                        serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
                    {
                        let stale: Vec<String> = rows
                            .iter()
                            .filter_map(|r| {
                                let id = r.get("id")?.as_str()?;
                                let p = r.get("rollout_path")?.as_str()?;
                                (!std::path::Path::new(p).exists())
                                    .then(|| format!("'{id}'"))
                            })
                            .collect();
                        if !stale.is_empty() {
                            let ds = format!(
                                "DELETE FROM threads WHERE id IN ({});",
                                stale.join(",")
                            );
                            let _ = std::process::Command::new("sqlite3")
                                .args(["-cmd", ".timeout 5000"])
                                .arg(&db)
                                .arg(&ds)
                                .output();
                            let hist = home.join(".codex/thread_history_1.sqlite");
                            if hist.exists() {
                                let hsql = format!(
                                    "DELETE FROM thread_items WHERE thread_id IN ({}); \
                                     DELETE FROM thread_turns WHERE thread_id IN ({}); \
                                     DELETE FROM thread_history_projection_state WHERE thread_id IN ({});",
                                    stale.join(","),
                                    stale.join(","),
                                    stale.join(",")
                                );
                                let _ = std::process::Command::new("sqlite3")
                                    .args(["-cmd", ".timeout 5000"])
                                    .arg(&hist)
                                    .arg(&hsql)
                                    .output();
                            }
                        }
                    }
                }
                if !ids.is_empty() {
                    let hist = home.join(".codex/thread_history_1.sqlite");
                    if hist.exists() {
                        let hsql = format!(
                            "DELETE FROM thread_items WHERE thread_id IN ({ids}); \
                             DELETE FROM thread_turns WHERE thread_id IN ({ids}); \
                             DELETE FROM thread_history_projection_state WHERE thread_id IN ({ids});"
                        );
                        let _ = std::process::Command::new("sqlite3")
                            .args(["-cmd", ".timeout 5000"])
                            .arg(&hist)
                            .arg(&hsql)
                            .output();
                    }
                }
            }
        }
        // OpenCode's list JSON omits slug, so get durable import IDs from its DB
        // and delete them in one bounded transaction through its DB command.
        let import_ids = crate::scan::opencode_import_ids()?;
        if dry_run {
            removed += import_ids.len();
        } else {
            removed += delete_opencode_imports(&import_ids)?;
        }
        // Heuristic removal can't be mapped back to keys: drop all of them so
        // future syncs re-import instead of trusting stale fingerprints.
        if !dry_run {
            keep.clear();
            let f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&p)?;
            drop(f);
        }
    }
    Ok(removed)
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

/// Extract every source to IR in parallel (bounded by CPU count; CLI-backed
/// sources are process-bound so threads still overlap their latency).
fn canonicalize_all(sources: &[&Session]) -> Vec<(Tool, String, Result<CanonicalSession>)> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let chunk = sources.len().div_ceil(threads).max(1);
    let mut results: Vec<(Tool, String, Result<CanonicalSession>)> =
        Vec::with_capacity(sources.len());
    std::thread::scope(|s| {
        let mut handles = vec![];
        for part in sources.chunks(chunk) {
            handles.push(s.spawn(move || {
                part.iter()
                    .map(|src| (src.tool, src.id.clone(), refs::canonicalize(src, &src.path)))
                    .collect::<Vec<_>>()
            }));
        }
        for h in handles {
            if let Ok(part) = h.join() {
                results.extend(part);
            }
        }
    });
    results
}

/// All-to-all sync. Empty `targets` => every writable tool.
/// Sessions imported by xses are skipped as sources (echo protection).
pub fn sync(
    targets: &[Tool],
    sessions: &[Session],
    dry_run: bool,
    include_imports: bool,
    scope: Option<usize>,
) -> Result<SyncReport> {
    let _lock = acquire_index_lock()?;
    let targets: Vec<Tool> = if targets.is_empty() {
        Tool::write_targets().to_vec()
    } else {
        targets.to_vec()
    };

    // Sources: everything except xses's own imports (unless overridden).
    let sources: Vec<&Session> = sessions
        .iter()
        .filter(|s| include_imports || !s.is_import)
        .collect();
    if sources.is_empty() {
        return Ok(SyncReport { targets: vec![] });
    }

    let seen = load_index()?;
    let needed_sources: Vec<&Session> = sources
        .iter()
        .filter(|src| {
            targets
                .iter()
                .any(|target| *target != src.tool && !seen.contains(&fingerprint(*target, src)))
        })
        .copied()
        .collect();
    let extracted = canonicalize_all(&needed_sources);
    // Target files are independent. Keep destructive/index mutations behind a
    // narrow mutex, while expensive serialization and external imports run in
    // parallel. The snapshot prevents target workers from racing on `seen`.
    let seen_snapshot = seen.clone();
    let mutation_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let mut report = SyncReport { targets: vec![] };
    std::thread::scope(|scope_threads| -> Result<()> {
        let mut workers = Vec::new();
        for target in targets {
            let mutation_lock = std::sync::Arc::clone(&mutation_lock);
            let extracted_ref = &extracted;
            let sources_ref = &sources;
            let seen_ref = &seen_snapshot;
            workers.push(scope_threads.spawn(move || -> Result<TargetReport> {
                let mut trep = TargetReport {
                    target,
                    written: 0,
                    skipped: 0,
                    failed: vec![],
                    loss: LossReport::default(),
                };
                for (src_tool, src_id, res) in extracted_ref {
                    if *src_tool == target {
                        continue;
                    }
                    let Some(src) = sources_ref
                        .iter()
                        .find(|s| s.tool == *src_tool && s.id == *src_id)
                    else {
                        continue;
                    };
                    let key = fingerprint(target, src);
                    if seen_ref.contains(&key) {
                        trep.skipped += 1;
                        continue;
                    }
                    let canon = match res {
                        Ok(c) => c.clone(),
                        Err(e) => {
                            trep.failed
                                .push(format!("{} {}: {e}", src_tool.as_str(), src_id));
                            continue;
                        }
                    };
                    let canon = match scope {
                        Some(n) => build::scope_last_n(canon, n),
                        None => canon,
                    };
                    let (transferable, scoped_loss) = canon.messages();
                    if transferable.is_empty() {
                        trep.skipped += 1;
                        continue;
                    }
                    if dry_run {
                        trep.loss.merge(scoped_loss);
                        trep.written += 1;
                        continue;
                    }
                    {
                        let _guard = mutation_lock.lock().expect("sync mutation lock");
                        prune_prev_copies(target, *src_tool, src_id);
                    }
                    let (files, r) = build::build_session(target, &canon);
                    if files.is_empty() {
                        trep.failed.push(format!(
                            "{} {}: no writer output",
                            src_tool.as_str(),
                            src_id
                        ));
                        continue;
                    }
                    trep.loss.merge(r);
                    trep.written += 1;
                    let _guard = mutation_lock.lock().expect("sync mutation lock");
                    remember(&key, files)?;
                }
                Ok(trep)
            }));
        }
        for worker in workers {
            let completed = worker
                .join()
                .map_err(|_| anyhow::anyhow!("sync worker panicked"))??;
            report.targets.push(completed);
        }
        Ok(())
    })?;
    Ok(report)
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
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 2000];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains("\"xses\":")
        || head.contains("\"sessionizer\":")
        || head.contains("\"cli_version\":\"0.0.0-xses\"")
        || head.contains("\"cli_version\":\"0.0.0-sessionizer\"")
}

/// Delete every previously-imported copy of one (target, source) pair,
/// regardless of which mtime/size variant produced it, and drop those index
/// lines. This is what keeps re-syncs 1:1 instead of accumulating a stale
/// copy per source change.
pub fn prune_prev_copies(target: Tool, src_tool: Tool, src_id: &str) {
    let Ok(p) = index_path() else { return };
    let prefix = format!("{}|{}|{}|", target.as_str(), src_tool.as_str(), src_id);
    let Ok(f) = fs::File::open(&p) else { return };
    let mut opencode_ids: HashSet<String> = HashSet::new();
    let mut keep: Vec<String> = Vec::new();
    for line in BufReader::new(f).lines().map_while(|l| l.ok()) {
        if !line.starts_with(&prefix) {
            keep.push(line);
            continue;
        }
        let paths = line.split_once('\t').map(|(_, paths)| paths).unwrap_or("");
        for path in decode_paths(paths) {
            if let Some(id) = path.strip_prefix("opencode://") {
                // Batch all old OpenCode rows into one database transaction.
                // Spawning `opencode session delete` once per source made a
                // normal sync pay the full CLI startup cost repeatedly.
                opencode_ids.insert(id.to_string());
            } else {
                let fp = PathBuf::from(&path);
                let _ = fs::remove_file(&fp);
            }
        }
    }
    if !opencode_ids.is_empty() {
        let _ = delete_opencode_imports(&opencode_ids);
    }
    // rewrite the index without the pruned lines (best-effort)
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&p)
    {
        use std::io::Write;
        for l in &keep {
            let _ = writeln!(f, "{l}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
