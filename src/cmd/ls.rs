//! `xses ls`: newest few sessions in this directory, or `--all`.
use crate::model::Tool;
use crate::scope;
use std::io::Write as _;

pub fn run(tool: Option<Tool>, all: bool, count: bool, limit: Option<usize>) -> anyhow::Result<()> {
    if count {
        return counts();
    }
    let mut sessions = scope::load(all)?;
    if let Some(t) = tool {
        sessions.retain(|s| s.tool == t);
    }
    let sessions = scope::capped(sessions, scope::limit(all, limit));
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for s in &sessions {
        writeln!(
            w,
            "{:<6} {:<16} {:>4} {:<44} {}",
            s.tool.as_str(),
            s.mtime.format("%Y-%m-%d %H:%M"),
            s.messages,
            s.summary,
            s.cwd.display()
        )?;
    }
    println!("\n{} sessions", sessions.len());
    Ok(())
}

/// `ls --count`: per-tool tallies across every project (a health check, so it
/// deliberately ignores the directory scope and the row cap).
fn counts() -> anyhow::Result<()> {
    let sessions = crate::scan::scan_all(None)?;
    for tool in Tool::write_targets() {
        let n = sessions.iter().filter(|s| s.tool == *tool).count();
        if n > 0 || !tool.is_cli_backed() {
            println!("{:<8} {:>4} sessions", tool.as_str(), n);
        }
    }
    Ok(())
}
