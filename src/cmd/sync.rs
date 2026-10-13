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
}

pub fn run(args: Args) -> anyhow::Result<()> {
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
    } = args;
    let plan = refs::resolve_items(&items, &from, &to)?;
    let sessions = scope::load(all)?;
    // Sources: explicit refs select; empty refs mean the newest per tool.
    // Explicit picks always resolve (even to imports); bulk mode skips
    // imports unless asked, to prevent echo loops.
    let has_refs = !plan.refs.is_empty();
    let mut sources: Vec<Session> = if has_refs {
        let mut picked = Vec::new();
        for r in plan.refs.clone() {
            picked.push(refs::select(&sessions, r)?.clone());
        }
        dedupe(picked)
    } else {
        let mut bulk = select_bulk_sources(sessions, all, num)?;
        bulk.retain(|s| include_imports || !s.is_import);
        bulk
    };
    if !plan.from_tools.is_empty() {
        sources.retain(|s| plan.from_tools.contains(&s.tool));
    }
    let mut tool_targets = refs::tool_targets(&plan.dests);
    let format_targets = refs::format_targets(&plan.dests);
    if tool_targets.is_empty() && format_targets.is_empty() && !has_refs {
        // bare `sync`: all-to-all (sync() expands empty to write_targets)
        tool_targets = Tool::write_targets().to_vec();
    }
    if tool_targets.is_empty() && format_targets.is_empty() {
        for s in &sources {
            let canon = refs::canonicalize(s, &s.path)?;
            let canon = match turn_scope {
                Some(n) => crate::canonical::scope_last_n(canon, n),
                None => canon,
            };
            if canon.messages().0.is_empty() {
                continue;
            }
            preview(&canon);
        }
        return Ok(());
    }
    if !tool_targets.is_empty() {
        // explicit refs bypass the import filter (bulk mode keeps it)
        let report = sync::sync(
            &tool_targets,
            &sources,
            dry_run,
            include_imports || has_refs,
            turn_scope,
        )?;
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
    for fmt in format_targets {
        for s in &sources {
            let mut canon = refs::canonicalize(s, &s.path)?;
            if let Some(n) = turn_scope {
                canon = crate::canonical::scope_last_n(canon, n);
            }
            if canon.messages().0.is_empty() {
                eprintln!("skip {} {}: no transferable content", s.tool.as_str(), s.id);
                continue;
            }
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
    Ok(())
}
