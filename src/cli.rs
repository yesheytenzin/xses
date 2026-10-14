//! Command-line surface. Flags mirror TODO.md: `ls`/`pick` default to the
//! newest few sessions, `clean` defaults to the newest synced session, and
//! `--all` widens each command to every project.
use crate::model::Tool;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(ValueEnum, Clone, Copy)]
pub enum ToolArg {
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

/// Rows shown by `ls`/`pick` before `--all` or `-n` widen the view.
pub const DEFAULT_LIMIT: usize = 5;

#[derive(Subcommand)]
pub enum Cmd {
    /// Interactive picker across all tools; Enter resumes the chosen session.
    Pick {
        #[arg(short, long)]
        query: Option<String>,
        /// Show sessions from all directories, not just cwd.
        #[arg(long)]
        all: bool,
        /// Rows to list (0 = no cap). Default: 5, or everything with --all.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Also show sessions that are empty, unanswered, or failed.
        #[arg(long)]
        include_noise: bool,
    },
    /// Move sessions: `sync` (latest per source tool), `sync --all` (all),
    /// `sync claude`, `sync latest --to pi`, `sync --from claude --to pi`,
    /// `sync latest sharegpt -o chat.json`. Tokens parse as tool/format, else ref.
    Sync {
        /// Session refs (`latest`, `latest:N`, id prefix) and/or destinations
        /// (tool names, `sharegpt`|`openai`). Empty = all sources, all tools.
        items: Vec<String>,
        /// Source tools to draw from, and/or refs (`--from claude`, `--from latest`).
        #[arg(long)]
        from: Vec<String>,
        /// Force destination interpretation (tools and/or formats).
        #[arg(long)]
        to: Vec<String>,
        /// Use sessions from every directory (default: cwd only).
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
        /// Also re-import untouched copies (continued imports sync automatically).
        #[arg(long)]
        include_imports: bool,
        /// Output file for format targets (default: stdout).
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
        /// Also sync sessions that are empty, unanswered, or failed.
        #[arg(long)]
        include_noise: bool,
    },
    /// List sessions across tools (newest few in this directory).
    #[command(alias = "list")]
    Ls {
        #[arg(value_enum)]
        tool: Option<ToolArg>,
        /// Show sessions from all directories, not just cwd.
        #[arg(long)]
        all: bool,
        /// Print per-tool tallies (every project) instead of the table.
        #[arg(long)]
        count: bool,
        /// Rows to list (0 = no cap). Default: 5, or everything with --all.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Also list sessions that are empty, unanswered, or failed.
        #[arg(long)]
        include_noise: bool,
    },
    /// Report what is installed, what failed, and what xses has imported.
    Doctor {
        /// Count sessions from every project (default: this project).
        #[arg(long)]
        all: bool,
        /// Journal entries to show (newest first).
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Remove xses imports: the newest synced session here, or every import
    /// ever made with `--all`.
    Clean {
        /// Remove every session xses ever imported, in every project.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
        /// Also delete continued copies and copies with unknown history.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Parser)]
#[command(version, about = "Find, resume and import AI coding agent sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}
