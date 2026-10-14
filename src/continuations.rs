//! Baselines track native message identities and semantic blocks, not titles.
use crate::canonical::{Block, CanonicalSession};
use crate::model::{Session, Tool};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hasher;
use std::io::BufRead;
use std::path::{Path, PathBuf};

type Marks = BTreeMap<String, usize>;

#[derive(Serialize, Deserialize)]
struct Baseline {
    version: u32,
    tool: Tool,
    path: PathBuf,
    cwd: PathBuf,
    created: chrono::DateTime<chrono::Utc>,
    stamp: Option<(u64, chrono::DateTime<chrono::Utc>)>,
    #[serde(default)]
    marks: Marks,
    #[serde(default)]
    identities: BTreeSet<String>,
    #[serde(default)]
    signature: String,
}

pub fn is_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
        || path.to_string_lossy().starts_with("opencode://")
}

pub fn known_import(path: &Path) -> bool {
    Tool::all_tools()
        .iter()
        .any(|tool| read(*tool, path).ok().flatten().is_some())
}

fn receipt_path(tool: Tool, path: &Path) -> Result<PathBuf> {
    Ok(crate::index::path()?
        .with_file_name("import-baselines")
        .join(format!(
            "{}.json",
            crate::write::import_id_for(tool, &path.to_string_lossy())
        )))
}

fn read(tool: Tool, path: &Path) -> Result<Option<Baseline>> {
    let bytes = match std::fs::read(receipt_path(tool, path)?) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let baseline: Baseline = serde_json::from_slice(&bytes).context("invalid import baseline")?;
    anyhow::ensure!(
        baseline.tool == tool && baseline.path == path,
        "import baseline does not match {}",
        path.display()
    );
    // Old user-only baselines cannot prove that assistant/image work is safe.
    if baseline.version != 2 {
        return Ok(None);
    }
    Ok(Some(baseline))
}

fn marks(canonical: &CanonicalSession) -> Marks {
    let mut marks = Marks::new();
    for event in &canonical.events {
        if matches!(
            event.block,
            Block::Usage { .. }
                | Block::Context { .. }
                | Block::Compressed { .. }
                | Block::Gap { .. }
        ) {
            continue;
        }
        let key = crate::write::import_id_for(
            Tool::Pi,
            &serde_json::to_string(&event.block).unwrap_or_default(),
        );
        *marks.entry(key).or_default() += 1;
    }
    marks
}

/// Hash bytes while discovering native message IDs. Metadata-only rows and
/// compaction IDs are deliberately excluded from continuation selection.
fn file_signature(path: &Path) -> Result<(String, BTreeSet<String>)> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut identities = BTreeSet::new();
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        hash.write(&line);
        let Ok(row) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        let kind = row["type"].as_str().unwrap_or_default();
        if !matches!(kind, "message" | "user" | "assistant" | "response_item")
            || row["isMeta"].as_bool() == Some(true)
            || row["isCompactSummary"].as_bool() == Some(true)
        {
            continue;
        }
        if let Some(id) = row
            .get("uuid")
            .or_else(|| row.get("id"))
            .or_else(|| row.pointer("/payload/id"))
            .and_then(|v| v.as_str())
        {
            identities.insert(id.to_owned());
        }
    }
    Ok((format!("{:016x}", hash.finish()), identities))
}

impl Baseline {
    fn current_stamp(&self) -> Result<Option<(u64, chrono::DateTime<chrono::Utc>)>> {
        if self.tool.is_cli_backed() {
            return Ok(None);
        }
        let metadata = std::fs::metadata(&self.path)?;
        Ok(Some((metadata.len(), metadata.modified()?.into())))
    }

    fn current(&self) -> Result<(Marks, BTreeSet<String>, String)> {
        if self.tool == Tool::Opencode {
            let id = self.path.to_string_lossy();
            let snapshot = crate::extract::opencode::opencode_snapshot(
                id.strip_prefix("opencode://")
                    .context("invalid OpenCode locator")?,
            )?;
            let session = Session {
                id: id.into_owned(),
                tool: self.tool,
                cwd: self.cwd.clone(),
                mtime: self.created,
                created: self.created,
                size: snapshot.size,
                revision: None,
                messages: 0,
                summary: String::new(),
                path: self.path.clone(),
                is_import: true,
                noise: None,
            };
            let canonical = crate::extract::canonicalize_events(&session, snapshot.events);
            return Ok((marks(&canonical), snapshot.identities, snapshot.revision));
        }
        let (signature, identities) = file_signature(&self.path)?;
        let canonical =
            crate::extract::extract(self.tool, "baseline", &self.cwd, self.created, &self.path)?;
        Ok((marks(&canonical), identities, signature))
    }

    fn continued(&self) -> Result<bool> {
        // File stamps are a selection optimization only. Overwrite protection
        // always verifies actual bytes, even for same-size, same-time edits.
        if self.stamp.is_some() && self.stamp == self.current_stamp()? {
            return Ok(false);
        }
        let (marks, identities, _) = self.current()?;
        Ok(!identities.is_subset(&self.identities)
            || marks
                .iter()
                .any(|(key, count)| *count > self.marks.get(key).copied().unwrap_or(0)))
    }
}

pub fn status(session: &Session) -> Result<Option<bool>> {
    read(session.tool, &session.path)?
        .map(|baseline| baseline.continued())
        .transpose()
}

pub fn record(tool: Tool, paths: &[PathBuf], source: &CanonicalSession) -> Result<()> {
    for path in paths.iter().filter(|p| is_transcript(p)) {
        let mut baseline = Baseline {
            version: 2,
            tool,
            path: path.clone(),
            cwd: source.cwd.clone(),
            created: source.created,
            stamp: None,
            marks: Marks::new(),
            identities: BTreeSet::new(),
            signature: String::new(),
        };
        baseline.stamp = baseline.current_stamp()?;
        (baseline.marks, baseline.identities, baseline.signature) = baseline.current()?;
        anyhow::ensure!(
            baseline.stamp == baseline.current_stamp()?,
            "target changed while recording import baseline: {}",
            path.display()
        );
        crate::publication::write(&receipt_path(tool, path)?, &serde_json::to_vec(&baseline)?)?;
    }
    Ok(())
}

pub fn protect(tool: Tool, paths: &[PathBuf]) -> Result<()> {
    for path in paths.iter().filter(|p| is_transcript(p)) {
        if !tool.is_cli_backed() && !path.exists() {
            continue;
        }
        if tool.is_cli_backed()
            && !crate::scan::opencode_exists(
                path.to_string_lossy().trim_start_matches("opencode://"),
            )?
        {
            continue;
        }
        let baseline = read(tool, path)?.with_context(|| {
            format!(
                "unknown history at {}: missing or legacy baseline; existing copy was preserved",
                path.display()
            )
        })?;
        let signature = if tool.is_cli_backed() {
            baseline.current()?.2
        } else {
            file_signature(path)?.0
        };
        anyhow::ensure!(signature == baseline.signature,
            "continued copy at {} was preserved; sync its session ID explicitly to share its branch", path.display());
    }
    Ok(())
}
