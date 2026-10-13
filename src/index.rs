//! The sync index (`~/.local/share/xses/index.jsonl`): one line per
//! (target, source) pair xses has imported, keyed by fingerprint.
use crate::model::Tool;
use anyhow::{Context, Result};

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
        .map_while(std::result::Result::ok)
        .collect();
    let mut out: Vec<(Tool, String)> = Vec::new();
    for line in lines.iter().rev() {
        let Some(key) = line.split('\t').next() else {
            continue;
        };
        if let Some(src) = source_of(key) {
            if !out.contains(&src) {
                out.push(src);
            }
        }
    }
    Ok(out)
}
