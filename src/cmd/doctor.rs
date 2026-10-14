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
    for tool in Tool::all_tools() {
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
            scope::format_noise(&noise)
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
    let (artifacts, stale, unmarked) = artifact_health()?;
    println!("  artifacts {artifacts}");
    if stale > 0 {
        println!("  stale     {stale} index records point at missing artifacts");
    }
    if unmarked > 0 {
        println!(
            "  unmarked  {unmarked} recorded files carry no import marker — possible native files claimed by the index (left alone; inspect before cleaning)"
        );
    }
    Ok(())
}

/// One pass over the index: recorded artifacts, how many point at
/// artifacts that no longer exist, and how many point at files with no
/// import marker (inline or sidecar) — a record claiming a native file
/// must never be cleaned on blind trust.
fn artifact_health() -> anyhow::Result<(usize, usize, usize)> {
    let p = index::path()?;
    if !p.exists() {
        return Ok((0, 0, 0));
    }
    use std::io::BufRead;
    let mut recorded = 0;
    let mut stale = 0;
    let mut unmarked = 0;
    for line in std::io::BufReader::new(std::fs::File::open(p)?)
        .lines()
        .map_while(Result::ok)
    {
        let Some((_, paths)) = line.split_once('\t') else {
            continue;
        };
        for path in crate::index::decode_paths(paths) {
            recorded += 1;
            let missing = match path.strip_prefix("opencode://") {
                Some(id) => id.is_empty(),
                None => !Path::new(&path).exists(),
            };
            stale += usize::from(missing);
            // Existing file artifacts must read as imports (inline marker
            // or sidecar); otherwise the record may claim a native file.
            // opencode:// ids and sidecars themselves are skipped: the
            // former live in SQLite, the latter are provenance by shape.
            if !missing
                && !path.starts_with("opencode://")
                && !path.ends_with(".xses.json")
                && Path::new(&path).extension().and_then(|e| e.to_str()) == Some("jsonl")
                && !crate::sync::first_line_has_marker(Path::new(&path))
                && crate::write::sidecar_provenance(Path::new(&path)).is_none()
            {
                unmarked += 1;
            }
        }
    }
    Ok((recorded, stale, unmarked))
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
        let (line, truncated) = format_issue_line(&issue, width);
        cut |= truncated;
        println!("{line}");
    }
    if cut {
        if let Some(path) = crate::issues::path() {
            println!("  (full text: {})", path.display());
        }
    }
}

/// One journal entry → one printable line. Pure (no terminal, no clock
/// beyond the entry's own stamp) so tests pin it with fixture outputs.
/// Returns the line plus whether the detail was truncated.
///
/// Journalled text is verbatim — another CLI's own output, escapes and
/// all — so classification stays structural (kind/subject), never a match
/// on upstream wording: only stripping and shortening touch the detail.
fn format_issue_line(issue: &crate::issues::Issue, width: usize) -> (String, bool) {
    let head = format!(
        "  {} {:<16} {} — ",
        issue.at.format("%m-%d %H:%M"),
        issue.kind,
        crate::ansi::strip(&issue.subject)
    );
    let budget = width.saturating_sub(head.chars().count()).max(24);
    let detail = crate::text::collapse(&crate::ansi::strip(&issue.detail));
    let line = crate::text::shorten(&detail, budget);
    let truncated = line != detail;
    (format!("{head}{line}"), truncated)
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
    use chrono::TimeZone as _;

    fn entry(kind: &str, subject: &str, detail: &str) -> crate::issues::Issue {
        crate::issues::Issue {
            at: chrono::Utc
                .with_ymd_and_hms(2026, 10, 13, 9, 30, 0)
                .unwrap(),
            kind: kind.into(),
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    /// Fixture outputs shaped like real journal rows (anonymized, trimmed
    /// to the shape — never matched verbatim in production).
    const SQLITE_LOCKED: &str =
        "exit status: 1 Error: database is locked | Failed query: insert into \"thread_items\"";
    const SQLITE_SCHEMA: &str =
        "exit status: 1 Error: Unexpected error | | Failed query: insert into \"message\"";
    const CLI_MISSING: &str =
        "launch sqlite3 for codex thread registration: No such file or directory";

    #[test]
    fn issue_details_become_one_short_line() {
        for detail in [SQLITE_LOCKED, SQLITE_SCHEMA, CLI_MISSING] {
            // Width 96: the head (~41 chars) leaves room for a truncated detail.
            let (line, truncated) = format_issue_line(&entry("codex-seed", "ses_abc", detail), 96);
            assert_eq!(line.lines().count(), 1, "{line}");
            assert!(line.contains("ses_abc"), "{line}");
            assert!(!line.contains('\x1b'), "{line}");
            // Long details truncate with an ellipsis marker.
            assert!(truncated, "{line}");
            assert!(line.chars().count() <= 96, "{line}");
        }
        // Short details pass through unmarked.
        let (line, truncated) =
            format_issue_line(&entry("opencode-import", "ses_abc", "exit status: 1"), 80);
        assert!(!truncated, "{line}");
        assert!(line.ends_with("exit status: 1"), "{line}");
    }

    #[test]
    fn ansi_escapes_never_reach_the_report_line() {
        let (line, _) = format_issue_line(
            &entry("k", "\x1b[31mses_abc\x1b[0m", "\x1b[1mdetail\x1b[0m"),
            80,
        );
        assert!(!line.contains('\x1b'), "{line}");
    }
}
