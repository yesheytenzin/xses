//! `xses doctor`: what is installed, what is recorded, and what failed.
//!
//! Everything here is read-only: the journal (`crate::issues`), the import
//! index, the scan cache, and the agent CLIs themselves.
use crate::index;
use crate::model::Tool;
use crate::scope;
use std::path::Path;

pub fn run(all: bool, limit: usize) -> anyhow::Result<()> {
    dependencies();
    stores(all)?;
    imports()?;
    issues(limit);
    Ok(())
}

/// Agent CLIs (and the sqlite3 the readers need) — what sync/resume can use.
fn dependencies() {
    println!("dependencies");
    let sqlite = has_command("sqlite3", &["--version"]);
    println!(
        "  {:<9} {}",
        "sqlite3",
        if sqlite {
            "ok"
        } else {
            "MISSING (session reads disabled)"
        }
    );
    for tool in Tool::write_targets() {
        let bin = resume_binary(*tool);
        println!(
            "  {:<9} {}",
            tool.as_str(),
            if has_command(bin, &["--version"]) {
                "ok"
            } else {
                "not installed (imports into it will fail)"
            }
        );
    }
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

fn has_command(bin: &str, args: &[&str]) -> bool {
    std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Sessions visible per store, and how many of them xses wrote.
/// Hidden-session reasons for these rows.
fn sessions_of(rows: &[&crate::model::Session]) -> Vec<crate::model::Session> {
    rows.iter().map(|s| (*s).clone()).collect()
}

fn stores(all: bool) -> anyhow::Result<()> {
    let sessions = scope::load(all, true)?;
    println!(
        "\nstores ({})",
        if all { "every project" } else { "this project" }
    );
    for tool in Tool::write_targets() {
        let rows: Vec<_> = sessions.iter().filter(|s| s.tool == *tool).collect();
        let imports = rows.iter().filter(|s| s.is_import).count();
        let noise = scope::noise_tally(&sessions_of(&rows));
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

/// Recent failures from the journal (the newest `limit`), grouped by kind.
fn issues(limit: usize) {
    let recent = crate::issues::recent(limit);
    println!("\nissues (newest {})", recent.len());
    if recent.is_empty() {
        println!("  none recorded");
        return;
    }
    for issue in recent {
        println!(
            "  {} {:<16} {} — {}",
            issue.at.format("%m-%d %H:%M"),
            issue.kind,
            issue.subject,
            issue.detail.lines().next().unwrap_or("")
        );
    }
}
