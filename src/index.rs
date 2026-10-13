//! The sync index (`~/.local/share/xses/index.jsonl`): one line per
//! (target, source) pair xses has imported, keyed by fingerprint.
use crate::model::Tool;
use anyhow::{Context, Result};

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub fn path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("no data dir")?
        .join("xses/index.jsonl"))
}

/// `target|src_tool|src_id|mtime|size` → the source session it came from.
pub fn source_of(key: &str) -> Option<(Tool, String)> {
    let mut parts = key.split('|');
    parts.next()?; // target
    let tool = Tool::parse(parts.next()?)?;
    let id = parts.next()?.to_string();
    (!id.is_empty()).then_some((tool, id))
}

/// Source sessions recorded in the index, most recently written first.
pub fn sources_newest_first() -> Result<Vec<(Tool, String)>> {
    let p = path()?;
    if !p.exists() {
        return Ok(Vec::new());
    }
    let lines: Vec<String> = BufReader::new(std::fs::File::open(&p)?)
        .lines()
        .collect::<std::io::Result<_>>()?;
    let mut out: Vec<(Tool, String)> = Vec::new();
    let mut seen = HashSet::new();
    for line in lines.iter().rev() {
        let Some(key) = line.split('\t').next() else {
            continue;
        };
        if let Some(src) = source_of(key) {
            if seen.insert(src.clone()) {
                out.push(src);
            }
        }
    }
    Ok(out)
}

#[derive(Clone)]
pub struct Record {
    pub key: String,
    pub paths: Vec<PathBuf>,
}

impl Record {
    pub fn pair(&self) -> String {
        self.key.split('|').take(3).collect::<Vec<_>>().join("|")
    }
}

/// Keep previous generations durably recorded until retirement succeeds.
/// A crash between publication and retirement is recovered on the next sync.
pub struct Index {
    pub(crate) path: PathBuf,
    pub(crate) records: Vec<Record>,
}

impl Index {
    pub fn load() -> Result<Self> {
        let path = path()?;
        let mut records = Vec::new();
        if path.exists() {
            for line in BufReader::new(std::fs::File::open(&path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let (key, paths) = line.split_once('\t').context("invalid sync index record")?;
                records.push(Record {
                    key: key.to_owned(),
                    paths: decode_paths(paths).into_iter().map(PathBuf::from).collect(),
                });
            }
        }
        Ok(Self { path, records })
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn remove_sources(&mut self, only: Option<&HashSet<(Tool, String)>>) -> Result<()> {
        self.records.retain(|r| {
            only.is_some_and(|selected| !source_of(&r.key).is_some_and(|s| selected.contains(&s)))
        });
        self.save()
    }

    pub fn keys(&self) -> HashSet<String> {
        let mut latest = HashMap::new();
        for record in &self.records {
            latest.insert(record.pair(), record.key.clone());
        }
        latest.into_values().collect()
    }

    fn save(&self) -> Result<()> {
        let mut bytes = Vec::new();
        use std::io::Write;
        for r in &self.records {
            let paths: Vec<_> = r.paths.iter().map(|p| p.to_string_lossy()).collect();
            writeln!(bytes, "{}\t{}", r.key, serde_json::to_string(&paths)?)?;
        }
        crate::storage::atomic_write(&self.path, &bytes)
    }

    pub fn commit(&mut self, updates: Vec<Record>) -> Result<()> {
        if !updates.is_empty() {
            self.records.extend(updates);
            // Publish references to the new generations before deleting any old
            // files. Both generations stay discoverable after a power failure.
            self.save()?;
        }
        let mut latest = HashMap::new();
        for (i, record) in self.records.iter().enumerate() {
            latest.insert(record.pair(), i);
        }
        let mut retired = HashSet::new();
        for (i, old) in self.records.iter().enumerate() {
            let new = &self.records[latest[&old.pair()]];
            if i == latest[&old.pair()] {
                continue;
            }
            let obsolete: Vec<_> = old
                .paths
                .iter()
                .filter(|p| !new.paths.contains(p))
                .cloned()
                .collect();
            match crate::sync::retire_artifacts(&obsolete) {
                Ok(()) => {
                    retired.insert(i);
                }
                Err(e) => crate::issues::record("retire-import", &old.key, &e.to_string()),
            }
        }
        if !retired.is_empty() {
            let mut i = 0;
            self.records.retain(|_| {
                let keep = !retired.contains(&i);
                i += 1;
                keep
            });
            self.save()?;
        }
        Ok(())
    }
}

pub fn decode_paths(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_else(|_| {
        raw.split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_precedes_retirement_and_pending_retirements_recover() {
        let dir = std::env::temp_dir().join(format!("xses-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.jsonl");
        let new = dir.join("new.jsonl");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        let old_record = Record {
            key: "pi|claude|source|old".into(),
            paths: vec![old.clone()],
        };
        let new_record = Record {
            key: "pi|claude|source|new".into(),
            paths: vec![new.clone()],
        };
        let mut index = Index {
            path: dir.join("index.jsonl"),
            records: vec![old_record, new_record],
        };
        // Simulate restart after both generations were durably published.
        index.save().unwrap();
        index.commit(Vec::new()).unwrap();
        assert!(!old.exists());
        assert_eq!(std::fs::read(new).unwrap(), b"new");
        assert_eq!(index.records.len(), 1);
        assert_eq!(index.keys(), HashSet::from(["pi|claude|source|new".into()]));
        assert!(!std::fs::read_to_string(&index.path)
            .unwrap()
            .contains("|old"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
