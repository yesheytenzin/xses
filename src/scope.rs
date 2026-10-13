//! Directory scope shared by ls/pick/sync/clean.
//!
//! Every command works on the current project by default and on everything
//! with `--all`; `limit`/`capped` give listings their default newest-first cap.
use crate::model::Session;
use std::path::PathBuf;

pub fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// Scan filter for a command's `--all` flag.
pub fn filter(all: bool) -> Option<PathBuf> {
    (!all).then(cwd)
}

/// Sessions visible to a command: this directory, or every project.
pub fn load(all: bool) -> anyhow::Result<Vec<Session>> {
    let f = filter(all);
    crate::scan::scan_all(f.as_deref())
}

/// Effective row cap: `--all` shows everything, `-n 0` means no cap.
pub fn limit(all: bool, requested: Option<usize>) -> Option<usize> {
    if all {
        return None;
    }
    match requested {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(crate::cli::DEFAULT_LIMIT),
    }
}

/// Newest-first rows, capped when a limit applies.
pub fn capped(mut sessions: Vec<Session>, cap: Option<usize>) -> Vec<Session> {
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    if let Some(n) = cap {
        sessions.truncate(n);
    }
    sessions
}
