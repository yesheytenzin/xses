//! `xses pick` / bare `xses`: interactive picker, Enter resumes.
use super::exec_resume;
use crate::picker;
use crate::scope;

pub fn run(
    query: String,
    all: bool,
    limit: Option<usize>,
    include_noise: bool,
) -> anyhow::Result<()> {
    let sessions = scope::capped(scope::load(all, include_noise)?, scope::limit(all, limit));
    if sessions.is_empty() {
        eprintln!("no sessions found (try --all)");
        return Ok(());
    }
    if let Some(s) = picker::picker(&sessions, &query)? {
        exec_resume(&s);
    }
    Ok(())
}
