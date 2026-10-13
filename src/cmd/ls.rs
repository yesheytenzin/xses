//! `xses ls`: newest few sessions in this directory, or `--all`.
//!
//! Sessions that were never useful (empty, unanswered, or failed) stay hidden
//! unless `--include-noise` is passed; `--count` always reports how many are
//! hidden, so they are never invisible.
use crate::model::Tool;
use crate::scope;
use std::io::Write as _;

pub fn run(
    tool: Option<Tool>,
    all: bool,
    count: bool,
    limit: Option<usize>,
    include_noise: bool,
) -> anyhow::Result<()> {
    if count {
        return counts();
    }
    let tools = tool
        .map(|t| vec![t])
        .unwrap_or_else(|| Tool::write_targets().to_vec());
    let mut sessions = crate::scan::scan_tools(scope::filter(all).as_deref(), &tools, true)?;
    let hidden = scope::noise_tally(&sessions);
    if !include_noise {
        sessions.retain(|s| s.noise.is_none());
    }
    let sessions = scope::capped(sessions, scope::limit(all, limit));
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for s in &sessions {
        let mark = match s.noise.as_deref() {
            Some(reason) => format!("!{reason}"),
            None => String::new(),
        };
        writeln!(
            w,
            "{:<6} {:<16} {:>4} {:<44} {} {}",
            s.tool.as_str(),
            s.mtime.format("%Y-%m-%d %H:%M"),
            s.messages,
            s.summary,
            s.cwd.display(),
            mark
        )?;
    }
    println!("\n{} sessions", sessions.len());
    if include_noise {
        return Ok(());
    }
    if !hidden.is_empty() {
        println!(
            "({} hidden: {}) — pass --include-noise to see them",
            hidden.iter().map(|(_, n)| n).sum::<usize>(),
            hidden
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
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
    let noise = scope::noise_tally(&sessions);
    if !noise.is_empty() {
        println!(
            "hidden   {:>4} not worth resuming ({})",
            noise.iter().map(|(_, n)| n).sum::<usize>(),
            noise
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
