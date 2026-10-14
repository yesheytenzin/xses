//! `xses sync`: move sessions between agent stores and to trace formats.
use super::{dedupe, export_path, preview, preview_format, select_bulk_sources};

use crate::model::{Session, Tool};
use crate::refs;
use crate::scope;
use crate::sync;
use std::io::Write as _;
use std::path::PathBuf;

pub struct Args {
    pub items: Vec<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub all: bool,
    pub num: Option<usize>,
    pub dry_run: bool,
    pub scope: Option<usize>,
    pub include_imports: bool,
    pub out: Option<PathBuf>,
    pub include_noise: bool,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    let _operation = sync::prepare(args.dry_run)?;
    let Args {
        items,
        from,
        to,
        all,
        num,
        dry_run,
        scope: turn_scope,
        include_imports,
        out,
        include_noise,
    } = args;
    let plan = refs::resolve_items(&items, &from, &to)?;
    let tools = if plan.from_tools.is_empty() {
        Tool::all_tools()
    } else {
        &plan.from_tools
    };
    let mut sessions = crate::scan::scan_tools(scope::filter(all).as_deref(), tools, !dry_run)?;
    if !include_noise {
        sessions.retain(|s| s.noise.is_none());
    }
    // Sources: explicit refs select; empty refs mean the newest per tool.
    // Explicit picks always resolve. Bulk mode also includes imported copies
    // with new local user turns, but excludes untouched copies to prevent echoes.
    let has_refs = !plan.refs.is_empty();
    let mut selection_failed = false;
    let sources: Vec<Session> = if has_refs {
        let mut picked = Vec::new();
        for r in plan.refs.clone() {
            picked.push(refs::select(&sessions, r)?.clone());
        }
        dedupe(picked)
    } else {
        let mut eligible = Vec::new();
        let mut legacy = 0;
        for session in sessions {
            if include_imports || !session.is_import {
                eligible.push(session);
            } else {
                match crate::continuations::status(&session) {
                    Ok(Some(true)) => eligible.push(session),
                    Ok(Some(false)) => {}
                    Ok(None) => legacy += 1,
                    Err(e) => {
                        let detail = format!("{}: {e:#}", session.short());
                        if !dry_run {
                            crate::issues::record("baseline-unreadable", &session.id, &detail);
                        }
                        eprintln!("preserved {detail}");
                        selection_failed = true;
                    }
                }
            }
        }
        if legacy > 0 {
            eprintln!("note: {legacy} older imports have no continuation baseline; sync a continued session by ID explicitly");
        }
        select_bulk_sources(eligible, all, num, true)?
    };
    let mut tool_targets = refs::tool_targets(&plan.dests);
    let format_targets = refs::format_targets(&plan.dests);
    if tool_targets.is_empty() && format_targets.is_empty() && !has_refs {
        // bare `sync`: all-to-all (sync() expands empty to all_tools)
        tool_targets = Tool::all_tools().to_vec();
    }
    if tool_targets.is_empty() && format_targets.is_empty() {
        for s in &sources {
            let canon = crate::extract::canonicalize(s)?;
            let canon = match turn_scope {
                Some(n) => crate::scope::last_n(canon, n),
                None => canon,
            };
            if !canon.transfer_summary().0 {
                continue;
            }
            preview(&canon);
        }
        return Ok(());
    }
    let mut import_failed = selection_failed;
    if !tool_targets.is_empty() {
        // explicit refs bypass the import filter (bulk mode keeps it)
        let progress = if dry_run {
            crate::progress::Progress::disabled()
        } else {
            crate::progress::Progress::new(0)
        };
        let report = sync::sync(
            &tool_targets,
            &sources,
            dry_run,
            true, // sources already selected above, including continued copies
            turn_scope,
            &progress,
        )?;
        progress.finish();
        import_failed |= report.targets.iter().any(|t| !t.failed.is_empty());
        let (written, skipped, loss) = report.totals();
        println!(
            "{}: {} source sessions across {} targets → {} written, {} skipped | fidelity: {} preserved, {} normalized, {} downgraded, {} dropped",
            if dry_run { "dry-run" } else { "sync" },
            sources.len(),
            report.targets.len(),
            written,
            skipped,
            loss.preserved,
            loss.normalized,
            loss.downgraded,
            loss.total_lost(),
        );
        for t in &report.targets {
            if t.written > 0 || !t.failed.is_empty() {
                println!(
                    "  {:<8} {} written, {} failed",
                    t.target.as_str(),
                    t.written,
                    t.failed.len()
                );
            }
            for f in &t.failed {
                eprintln!("    fail {f}");
            }
        }
        // `sync --from claude --to claude` reads like a mistake; say so rather
        // than printing a zero row that looks like a failure.
        if written == 0
            && skipped == 0
            && !sources.is_empty()
            && sources.iter().all(|s| tool_targets.contains(&s.tool))
        {
            let mut tools: Vec<&str> = sources.iter().map(|s| s.tool.as_str()).collect();
            tools.sort_unstable();
            tools.dedup();
            println!(
                "note: every source ({}) is already one of the targets — xses never imports a session into its own tool",
                tools.join(", ")
            );
        }
    }
    let multi_export = sources.len() > 1 || format_targets.len() > 1;
    for s in &sources {
        if format_targets.is_empty() {
            break;
        }
        let mut canon = crate::extract::canonicalize(s)?;
        if let Some(n) = turn_scope {
            canon = crate::scope::last_n(canon, n);
        }
        if !canon.transfer_summary().0 {
            eprintln!("skip {} {}: no transferable content", s.tool.as_str(), s.id);
            continue;
        }
        for fmt in &format_targets {
            if dry_run {
                preview_format(&canon, fmt);
                continue;
            }
            let bytes = crate::write::export_trace(&canon, fmt)?;
            match &out {
                Some(p) => {
                    let dest = export_path(p, fmt, s, multi_export);
                    std::fs::write(&dest, &bytes)?;
                    println!("wrote {}", dest.display());
                }
                None => std::io::stdout().write_all(&bytes)?,
            }
        }
    }
    anyhow::ensure!(
        !import_failed,
        "one or more imports failed; run xses doctor for details"
    );
    Ok(())
}
