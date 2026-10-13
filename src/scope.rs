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
///
/// Sessions classified as `empty`/`unanswered`/`failed` are hidden unless the
/// caller opts in — they were never useful to resume, and importing them fills
/// every other agent's picker with noise.
pub fn load(all: bool, include_noise: bool) -> anyhow::Result<Vec<Session>> {
    let f = filter(all);
    let mut sessions = crate::scan::scan_all(f.as_deref())?;
    if !include_noise {
        sessions.retain(|s| s.noise.is_none());
    }
    Ok(sessions)
}

/// Hidden-session tally by reason, for `ls --count` and `doctor`.
pub fn noise_tally(sessions: &[Session]) -> Vec<(&'static str, usize)> {
    let mut out: Vec<(&'static str, usize)> = Vec::new();
    for s in sessions {
        let Some(reason) = s.noise.as_deref() else {
            continue;
        };
        let key = match reason {
            "empty" => "empty",
            "unanswered" => "unanswered",
            _ => "failed",
        };
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += 1,
            None => out.push((key, 1)),
        }
    }
    out.sort();
    out
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
