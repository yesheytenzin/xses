//! xses — unified picker, cross-importer and converter for AI coding
//! agent sessions (claude, codex, pi, oh-my-pi).
mod build;
mod canonical;
mod extract;
mod model;
mod picker;
mod refs;
mod scan;
mod sync;

use clap::{Parser, Subcommand, ValueEnum};
use model::{Session, Tool};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(ValueEnum, Clone, Copy)]
enum ToolArg {
    Claude,
    Codex,
    Pi,
    Omp,
    Opencode,
}

impl From<ToolArg> for Tool {
    fn from(t: ToolArg) -> Tool {
        match t {
            ToolArg::Claude => Tool::Claude,
            ToolArg::Codex => Tool::Codex,
            ToolArg::Pi => Tool::Pi,
            ToolArg::Omp => Tool::Omp,
            ToolArg::Opencode => Tool::Opencode,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive picker across all tools; Enter resumes the chosen session.
    Pick {
        #[arg(short, long)]
        query: Option<String>,
        /// Show sessions from all directories, not just cwd.
        #[arg(long)]
        all: bool,
    },
    /// Move sessions: `sync` (latest per source tool), `sync --all` (all),
    /// `sync claude`, `sync latest --to pi`,
    /// `sync latest sharegpt -o chat.json`. Tokens parse as tool/format, else ref.
    Sync {
        /// Session refs (`latest`, `latest:N`, id prefix) and/or destinations
        /// (tool names, `sharegpt`|`openai`). Empty = all sources, all tools.
        items: Vec<String>,
        /// Force ref interpretation (escape hatch for tool-name-like ids).
        #[arg(long)]
        from: Vec<String>,
        /// Force destination interpretation (tools and/or formats).
        #[arg(long)]
        to: Vec<String>,
        /// Import sessions from every directory (default: cwd only).
        #[arg(long)]
        all: bool,
        /// Number of newest sessions to import per source tool (default: 1).
        #[arg(short = 'n', long = "num")]
        num: Option<usize>,
        #[arg(long)]
        dry_run: bool,
        /// Only convert the trailing N turns of each session.
        #[arg(long)]
        scope: Option<usize>,
        /// Also re-import files xses itself wrote (default: skip imports).
        #[arg(long)]
        include_imports: bool,
        /// Output file for format targets (default: stdout).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// List sessions across tools (non-interactive).
    #[command(alias = "list")]
    Ls {
        #[arg(value_enum)]
        tool: Option<ToolArg>,
        #[arg(long)]
        all: bool,
        /// Print per-tool tallies instead of the table.
        #[arg(long)]
        count: bool,
    },
    /// Remove every session xses imported (restores stores to native-only).
    Clean {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

fn export_path(
    base: &std::path::Path,
    fmt: &str,
    session: &Session,
    disambiguate: bool,
) -> PathBuf {
    if !disambiguate {
        return base.to_path_buf();
    }
    let parent = base.parent().unwrap_or_else(|| std::path::Path::new("."));
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

fn load(all: bool) -> anyhow::Result<Vec<Session>> {
    let filter = if all { None } else { Some(cwd()) };
    scan::scan_all(filter.as_deref())
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
    for tool in [
        Tool::Claude,
        Tool::Codex,
        Tool::Pi,
        Tool::Omp,
        Tool::Opencode,
    ] {
        let mut per_tool: Vec<Session> = sessions
            .iter()
            .filter(|s| s.tool == tool && !s.is_import)
            .cloned()
            .collect();
        per_tool.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        selected.extend(per_tool.into_iter().take(limit));
    }
    selected.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    Ok(selected)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None | Some(Cmd::Pick { .. }) => {
            let (query, all) = match cli.cmd {
                Some(Cmd::Pick { query, all }) => (query.unwrap_or_default(), all),
                _ => (String::new(), false),
            };
            let sessions = load(all)?;
            if sessions.is_empty() {
                eprintln!("no sessions found (try --all)");
                return Ok(());
            }
            if let Some(s) = picker::picker(&sessions, &query)? {
                exec_resume(&s);
            }
        }
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
        }) => {
            let (ref_list, dests) = refs::resolve_items(&items, &from, &to)?;
            let sessions = load(all)?;
            // sources: explicit refs select; empty refs mean everything.
            // Explicit refs always resolve (even to imports); bulk mode skips
            // imports unless asked, to prevent echo loops.
            let mut sources: Vec<Session> = if ref_list.is_empty() {
                select_bulk_sources(sessions, all, num)?
            } else {
                let mut picked = Vec::new();
                for r in ref_list.clone() {
                    picked.push(refs::select(&sessions, r)?.clone());
                }
                let mut seen = HashSet::new();
                picked.retain(|s| seen.insert((s.tool, s.id.clone())));
                picked
            };
            let has_refs = !ref_list.is_empty();
            if !has_refs {
                sources.retain(|s| include_imports || !s.is_import);
            }
            let mut tool_targets = refs::tool_targets(&dests);
            let format_targets = refs::format_targets(&dests);
            if tool_targets.is_empty() && format_targets.is_empty() && !has_refs {
                // bare `sync`: all-to-all (sync() expands empty to write_targets)
                tool_targets = Tool::write_targets().to_vec();
            }
            if tool_targets.is_empty() && format_targets.is_empty() {
                for s in &sources {
                    let canon = refs::canonicalize(s, &s.path)?;
                    let canon = match scope {
                        Some(n) => build::scope_last_n(canon, n),
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
                    scope,
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
            }
            let multi_export = sources.len() > 1 || format_targets.len() > 1;
            for fmt in format_targets {
                for s in &sources {
                    let mut canon = refs::canonicalize(s, &s.path)?;
                    if let Some(n) = scope {
                        canon = build::scope_last_n(canon, n);
                    }
                    if canon.messages().0.is_empty() {
                        eprintln!("skip {} {}: no transferable content", s.tool.as_str(), s.id);
                        continue;
                    }
                    if dry_run {
                        preview_format(&canon, fmt);
                        continue;
                    }
                    let bytes = build::export_trace(&canon, fmt)?;
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
        }
        Some(Cmd::Ls { tool, all, count }) => {
            if count {
                let sessions = scan::scan_all(None)?;
                let mut tally = [
                    (Tool::Claude, 0usize),
                    (Tool::Codex, 0),
                    (Tool::Pi, 0),
                    (Tool::Omp, 0),
                    (Tool::Opencode, 0),
                ];
                for s in &sessions {
                    let slot = match s.tool {
                        Tool::Claude => &mut tally[0].1,
                        Tool::Codex => &mut tally[1].1,
                        Tool::Pi => &mut tally[2].1,
                        Tool::Omp => &mut tally[3].1,
                        Tool::Opencode => &mut tally[4].1,
                    };
                    *slot += 1;
                }
                for (t, n) in tally {
                    if n > 0 || !t.is_cli_backed() {
                        println!("{:<8} {:>4} sessions", t.as_str(), n);
                    }
                }
                return Ok(());
            }
            let mut sessions = load(all)?;
            if let Some(t) = tool {
                let t: Tool = t.into();
                sessions.retain(|s| s.tool == t);
            }
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
        }
        Some(Cmd::Clean { dry_run }) => {
            let n = sync::clean(dry_run)?;
            println!(
                "{}: removed {} imported artifacts",
                if dry_run { "dry-run" } else { "clean" },
                n
            );
        }
    }
    Ok(())
}

/// Preview a selected session: what transfers, what's dropped.
fn preview(canon: &canonical::CanonicalSession) {
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
    let dropped = canon
        .blocks
        .iter()
        .filter(|b| !matches!(b, canonical::Block::Text { .. }))
        .count();
    if dropped > 0 {
        println!("dropped     {dropped} non-text blocks (tool calls/results/thinking will be flattened or lost depending on target)");
    }
}

fn preview_format(canon: &canonical::CanonicalSession, fmt: &str) {
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
