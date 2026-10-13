//! Issue journal: what failed, across runs, for `xses doctor`.
//!
//! Operations record failures here (a CLI that refused an import, an
//! unreadable record, a store that could not be seeded) instead of only
//! printing them once to a terminal nobody kept. Best effort by design: a
//! journal write must never break the operation that reported it.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Entries kept before the journal is trimmed to the newest half.
const MAX_LINES: usize = 400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

/// Journal location, for callers that need to point at the full text.
pub fn path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("xses").join("issues.jsonl"))
}

/// Append one issue (never fails the caller).
pub fn record(kind: &str, subject: &str, detail: &str) {
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    append(
        &p,
        &Issue {
            at: Utc::now(),
            kind: kind.to_string(),
            subject: subject.to_string(),
            detail: detail.trim().to_string(),
        },
    );
}

fn append(p: &Path, issue: &Issue) {
    let Ok(line) = serde_json::to_string(issue) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = writeln!(f, "{line}");
    }
    trim(p);
}

fn trim(p: &Path) {
    let Ok(file) = std::fs::File::open(p) else {
        return;
    };
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    if lines.len() <= MAX_LINES {
        return;
    }
    let keep = &lines[lines.len() - MAX_LINES / 2..];
    if let Ok(mut f) = std::fs::File::create(p) {
        for line in keep {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn read(p: &Path, limit: usize) -> Vec<Issue> {
    let Ok(file) = std::fs::File::open(p) else {
        return Vec::new();
    };
    let mut out: Vec<Issue> = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

/// Recent issues, newest first.
pub fn recent(limit: usize) -> Vec<Issue> {
    path().map(|p| read(&p, limit)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(subject: &str) -> Issue {
        Issue {
            at: Utc::now(),
            kind: "import".into(),
            subject: subject.into(),
            detail: "boom".into(),
        }
    }

    #[test]
    fn journal_reads_back_newest_first() {
        let p = std::env::temp_dir().join(format!("xses-issues-{}.jsonl", std::process::id()));
        std::fs::remove_file(&p).ok();
        append(&p, &issue("one"));
        append(&p, &issue("two"));
        let got = read(&p, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].subject, "two");
        assert_eq!(got[1].subject, "one");
        std::fs::remove_file(&p).ok();
    }
}
