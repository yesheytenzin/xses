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

/// Effective row cap. An explicit `-n` always wins (`-n 0` = no cap);
/// `--all` only drops the default cap, widening the view to every project.
pub fn limit(all: bool, requested: Option<usize>) -> Option<usize> {
    match requested {
        Some(0) => None,
        Some(n) => Some(n),
        None => (!all).then_some(crate::cli::DEFAULT_LIMIT),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_limit_beats_the_all_default() {
        // default: newest few of this project
        assert_eq!(limit(false, None), Some(crate::cli::DEFAULT_LIMIT));
        // an explicit count always wins, with or without --all
        assert_eq!(limit(false, Some(8)), Some(8));
        assert_eq!(limit(true, Some(8)), Some(8));
        // 0 means no cap
        assert_eq!(limit(false, Some(0)), None);
        assert_eq!(limit(true, Some(0)), None);
        // --all alone drops the default cap
        assert_eq!(limit(true, None), None);
    }
}
