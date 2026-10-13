//! JSONL record helpers shared by the store scanners and the extractors.
use crate::canonical::Block;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Read every non-empty record, invoking `f` per line; stops early when `f`
/// returns `false`. Returns the number of parsed records.
pub fn for_each_record(path: &Path, mut f: impl FnMut(&Value) -> bool) -> Result<u64> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    let mut n = 0u64;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            n += 1;
            if !f(&v) {
                break;
            }
        }
    }
    Ok(n)
}

/// Parse one record for an extractor. A truncated or corrupt line (partial
/// write from a crashed agent) becomes a visible gap instead of failing the
/// whole session.
pub fn parse_record(line_no: usize, line: &str, out: &mut Vec<Block>) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => Some(v),
        Err(_) => {
            out.push(Block::Gap {
                kind: "unreadable-record".into(),
                detail: format!("record at line {} could not be read", line_no + 1),
            });
            None
        }
    }
}

/// Head of a file, for provenance markers stored in the first record.
pub fn head_bytes(path: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}
