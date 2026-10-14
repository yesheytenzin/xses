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
        .unwrap_or_else(|| Tool::all_tools().to_vec());
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
            scope::format_noise(&hidden)
        );
    }
    Ok(())
}

/// `ls --count`: per-tool tallies across every project (a health check, so it
/// deliberately ignores the directory scope and the row cap).
fn counts() -> anyhow::Result<()> {
    let counts = crate::scan::counts::all()?;
    let mut empty = 0;
    let mut unanswered = 0;
    let mut failed = 0;
    for (tool, count) in counts {
        let n = count.total;
        empty += count.empty;
        unanswered += count.unanswered;
        failed += count.failed;
        if n > 0 || !tool.is_cli_backed() {
            println!("{:<8} {:>4} sessions", tool.as_str(), n);
        }
    }
    let noise: Vec<_> = [
        ("empty", empty),
        ("failed", failed),
        ("unanswered", unanswered),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .collect();
    if !noise.is_empty() {
        println!(
            "hidden   {:>4} not worth resuming ({})",
            noise.iter().map(|(_, n)| n).sum::<usize>(),
            scope::format_noise(&noise)
        );
    }
    Ok(())
}
