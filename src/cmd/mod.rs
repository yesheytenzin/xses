//! Command implementations. `main` parses and delegates here.
mod clean;
mod doctor;
mod ls;
mod pick;
mod sync;

use crate::canonical::CanonicalSession;
use crate::cli::{Cli, Cmd};
use crate::model::{Session, Tool};
use crate::picker;
use clap::Parser;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => pick::run(String::new(), false, None),
        Some(Cmd::Pick { query, all, limit }) => pick::run(query.unwrap_or_default(), all, limit),
        Some(Cmd::Sync {
            items,
            from,
            to,
            all,
            num,
            dry_run,
            scope,
            include_imports,
            out,
        }) => sync::run(sync::Args {
            items,
            from,
            to,
            all,
            num,
            dry_run,
            scope,
            include_imports,
            out,
        }),
        Some(Cmd::Ls {
            tool,
            all,
            count,
            limit,
        }) => ls::run(tool.map(Tool::from), all, count, limit),
        Some(Cmd::Doctor { all, limit }) => doctor::run(all, limit),
        Some(Cmd::Clean { all, dry_run }) => clean::run(all, dry_run),
    }
}

/// Output path for a format export; multiple selected sessions get distinct
/// names so `-o` never silently overwrites.
fn export_path(base: &Path, fmt: &str, session: &Session, disambiguate: bool) -> PathBuf {
    if !disambiguate {
        return base.to_path_buf();
    }
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("export");
    let id: String = session
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(24)
        .collect();
    let id = if id.is_empty() { "session" } else { &id };
    parent.join(format!("{stem}.{}-{id}.{fmt}", session.tool.as_str()))
}

/// Select the newest native sessions per source tool for bulk sync.
fn select_bulk_sources(
    sessions: Vec<Session>,
    all: bool,
    num: Option<usize>,
) -> anyhow::Result<Vec<Session>> {
    if matches!(num, Some(0)) {
        anyhow::bail!("-n/--num requires N >= 1");
    }
    let Some(limit) = (if all { None } else { Some(num.unwrap_or(1)) }) else {
        return Ok(sessions.into_iter().filter(|s| !s.is_import).collect());
    };
    let mut selected = Vec::new();
    for tool in Tool::write_targets() {
        let mut per_tool: Vec<Session> = sessions
            .iter()
            .filter(|s| s.tool == *tool && !s.is_import)
            .cloned()
            .collect();
        per_tool.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        selected.extend(per_tool.into_iter().take(limit));
    }
    selected.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    Ok(selected)
}

/// Preview a selected session: what transfers, what's dropped.
fn preview(canon: &CanonicalSession) {
    let (messages, report) = canon.messages();
    println!(
        "source      {} session {} ({})",
        canon.tool.as_str(),
        canon.id,
        canon.cwd.display()
    );
    println!("scope       {} turns", canon.message_count());
    println!(
        "transfer    {} text turns ({} preserved, {} normalized, {} downgraded)",
        messages.len(),
        report.preserved,
        report.normalized,
        report.downgraded
    );
    if report.dropped > 0 {
        println!(
            "dropped     {} blocks ({}) — shown as [gap] markers in the import",
            report.dropped,
            canon.gap_kinds().join(", ")
        );
    }
}

fn preview_format(canon: &CanonicalSession, fmt: &str) {
    let (messages, _report) = canon.messages();
    println!(
        "{} session {} → {}: {} turns (dry-run, not written)",
        canon.tool.as_str(),
        &canon.id[..canon.id.len().min(8)],
        fmt,
        messages.len()
    );
}

fn exec_resume(s: &Session) {
    let argv = picker::resume_argv(s);
    if argv.is_empty() {
        eprintln!("{} has no CLI resume — move it instead:", s.tool.as_str());
        eprintln!("  xses sync {} --to claude", s.locator());
        return;
    }
    let (bin, rest) = argv.split_first().expect("resume argv non-empty");
    eprintln!("▶ resuming {} ({})", s.short(), s.cwd.display());
    match std::process::Command::new(bin).args(rest).status() {
        Ok(st) if st.success() => {}
        Ok(st) => eprintln!("resume exited with {st}"),
        Err(e) => eprintln!("spawn {bin}: {e}"),
    }
}

/// Distinct (tool, id) pairs from a session list.
fn dedupe(sessions: Vec<Session>) -> Vec<Session> {
    let mut seen = HashSet::new();
    sessions
        .into_iter()
        .filter(|s| seen.insert((s.tool, s.id.clone())))
        .collect()
}
