//! `xses doctor`: what is recorded, and what failed.
//!
//! Read-only: the journal (`crate::issues`), the import index, the scan cache,
//! and the agents' own stores. Only installed agents are reported — a store
//! nobody can resume from is not health, it is clutter.
use crate::index;
use crate::model::Tool;
use crate::scope;
use std::path::Path;

/// Widest a report line may get, so a wide terminal still shows one line per
/// issue instead of a wall of text.
const MAX_LINE: usize = 140;

pub fn run(all: bool, limit: usize) -> anyhow::Result<()> {
    stores(all)?;
    imports()?;
    issues(limit);
    Ok(())
}

fn resume_binary(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "claude",
        Tool::Codex => "codex",
        Tool::Pi => "pi",
        Tool::Omp => "omp",
        Tool::Opencode => "opencode",
    }
}

/// Is `bin` an executable on `$PATH`? Resolution only — running `--version`
/// would exec another process (and report "missing" for a CLI whose runtime
/// is broken, which is health information, not absence).
fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

fn stores(all: bool) -> anyhow::Result<()> {
    println!(
        "stores ({})",
        if all { "every project" } else { "this project" }
    );
    let filter = scope::filter(all);
    let mut shown = 0;
    for tool in Tool::write_targets() {
        // Only installed agents: an agent nobody can resume in has no store
        // worth reporting, and its reader cannot run.
        if !on_path(resume_binary(*tool)) {
            continue;
        }
        shown += 1;
        // One store at a time, so an unreadable store reports itself instead
        // of taking the whole report down.
        let sessions = match crate::scan::scan_tools(filter.as_deref(), &[*tool], false) {
            Ok(sessions) => sessions,
            Err(e) => {
                println!("  {:<9} unavailable: {e}", tool.as_str());
                continue;
            }
        };
        let rows: Vec<&crate::model::Session> = sessions.iter().collect();
        let imports = rows.iter().filter(|s| s.is_import).count();
        let noise = scope::noise_tally(&sessions);
        println!(
            "  {:<9} {:>5} sessions ({} imported){}{}",
            tool.as_str(),
            rows.len(),
            imports,
            if noise.is_empty() { "" } else { " | hidden: " },
            noise
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if shown == 0 {
        println!("  no agent CLIs on PATH");
    }
    Ok(())
}

/// Index health: recorded imports whose artifacts are gone.
fn imports() -> anyhow::Result<()> {
    let sources = index::sources_newest_first()?;
    let path = index::path()?;
    println!("\nimports");
    println!("  index     {}", path.display());
    println!("  recorded  {} source sessions", sources.len());
    println!("  artifacts {}", recorded_artifacts()?);
    let stale = stale_artifacts()?;
    if stale > 0 {
        println!("  stale     {stale} index records point at missing artifacts");
    }
    Ok(())
}

fn index_lines() -> anyhow::Result<Vec<String>> {
    let p = index::path()?;
    if !p.exists() {
        return Ok(Vec::new());
    }
    use std::io::BufRead;
    Ok(std::io::BufReader::new(std::fs::File::open(p)?)
        .lines()
        .map_while(Result::ok)
        .collect())
}

fn recorded_artifacts() -> anyhow::Result<usize> {
    let mut n = 0;
    for line in index_lines()? {
        if let Some((_, paths)) = line.split_once('\t') {
            n += serde_json::from_str::<Vec<String>>(paths)
                .map(|v| v.len())
                .unwrap_or_else(|_| paths.split(',').filter(|s| !s.is_empty()).count());
        }
    }
    Ok(n)
}

fn stale_artifacts() -> anyhow::Result<usize> {
    let mut stale = 0;
    for line in index_lines()? {
        let Some((_, paths)) = line.split_once('\t') else {
            continue;
        };
        let Ok(paths) = serde_json::from_str::<Vec<String>>(paths) else {
            continue;
        };
        for path in paths {
            if let Some(id) = path.strip_prefix("opencode://") {
                if id.is_empty() {
                    stale += 1;
                }
                continue;
            }
            if !Path::new(&path).exists() {
                stale += 1;
            }
        }
    }
    Ok(stale)
}

/// Recent failures from the journal (the newest `limit`), one line each: the
/// subject plus the start of the detail, collapsed and cut to fit the screen.
fn issues(limit: usize) {
    let recent = crate::issues::recent(limit);
    println!("\nissues (newest {})", recent.len());
    if recent.is_empty() {
        println!("  none recorded");
        return;
    }
    let width = report_width();
    let mut cut = false;
    for issue in recent {
        // Journalled text is verbatim (another CLI's own output, escapes and
        // all); a report line must not carry those escapes to the terminal.
        let head = format!(
            "  {} {:<16} {} — ",
            issue.at.format("%m-%d %H:%M"),
            issue.kind,
            crate::ansi::strip(&issue.subject)
        );
        let budget = width.saturating_sub(head.chars().count()).max(24);
        let detail = collapse(&crate::ansi::strip(&issue.detail));
        let (line, truncated) = clip(&detail, budget);
        cut |= truncated;
        println!("{head}{line}");
    }
    if cut {
        if let Some(path) = crate::issues::path() {
            println!("  (full text: {})", path.display());
        }
    }
}

/// One line: runs of whitespace (newlines included) become single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut to `width` characters, marking the cut. Returns whether it cut.
fn clip(s: &str, width: usize) -> (String, bool) {
    if s.chars().count() <= width {
        return (s.to_string(), false);
    }
    let kept: String = s.chars().take(width.saturating_sub(1)).collect();
    (format!("{}…", kept.trim_end()), true)
}

/// Line budget: the terminal's width, capped so one issue still reads as one
/// line on a very wide screen.
fn report_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(MAX_LINE)
        .min(MAX_LINE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_details_become_one_short_line() {
        assert_eq!(collapse("a\n b\t\tc "), "a b c");
        let sql = "exit status: 1 Error: Unexpected error | | Failed query: insert into \"message\" (\"id\", \"session_id\", \"time_created\", \"time_updated\", \"data\") values (?, ?, ?, ?, ?) on conflict do nothing | params: msg_000000-JpjBgqGMuMlfThIb3k8pbhJ3,ses_2072fc640e2442358c9d0e242072fc64,1791911116704";
        let (line, cut) = clip(&collapse(sql), 80);
        assert!(cut, "long details must be cut");
        assert!(
            line.chars().count() <= 80,
            "line must fit the budget: {} chars",
            line.chars().count()
        );
        assert!(line.chars().count() >= 78, "not a stub: {line}");
        assert!(line.ends_with('…'), "{line}");
        let (short, cut) = clip("exit status: 1", 80);
        assert!(!cut);
        assert_eq!(short, "exit status: 1");
    }
}
