//! Unified session record produced by every source scanner.
use chrono::{DateTime, Utc};
use std::path::PathBuf;
/// Serializes every `opencode` CLI invocation in this process. Its `export`
/// truncates stdout to 64KB with exit 0 when run concurrently.
pub static OPENCODE_CLI_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Pi,
    Omp,
    Opencode,
}

impl Tool {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Pi => "pi",
            Tool::Omp => "omp",
            Tool::Opencode => "opencode",
        }
    }

    pub fn parse(s: &str) -> Option<Tool> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Some(Tool::Claude),
            "codex" => Some(Tool::Codex),
            "pi" => Some(Tool::Pi),
            "omp" | "oh-my-pi" => Some(Tool::Omp),
            "opencode" => Some(Tool::Opencode),
            _ => None,
        }
    }

    /// Every target we can write into.
    pub fn write_targets() -> &'static [Tool] {
        &[
            Tool::Claude,
            Tool::Codex,
            Tool::Pi,
            Tool::Omp,
            Tool::Opencode,
        ]
    }

    /// True for sources that live behind a CLI/DB, not a plain file.
    pub fn is_cli_backed(&self) -> bool {
        matches!(self, Tool::Opencode)
    }
}

/// One session, tool-agnostic. `path` is the native transcript for file-backed
/// tools; for CLI-backed tools it's a synthetic `<tool>://<id>` locator.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// Native session id inside the source tool.
    pub id: String,
    pub tool: Tool,
    pub cwd: PathBuf,
    pub mtime: DateTime<Utc>,
    /// Source creation time (file birth / native created). Stable across
    /// appends — used for import naming and timestamps.
    pub created: DateTime<Utc>,
    pub size: u64,
    pub messages: u64,
    /// First user message text or native title, truncated.
    pub summary: String,
    pub path: PathBuf,
    /// True if this file was written by xses (echo protection).
    pub is_import: bool,
}

impl Session {
    /// Colon-free display id for prompt lines.
    pub fn short(&self) -> String {
        let id = self.id.as_str();
        let prefix: String = id.chars().take(8).collect();
        format!("{}@{}", prefix, self.tool.as_str())
    }

    pub fn locator(&self) -> String {
        if self.tool.is_cli_backed() {
            format!("{}://{}", self.tool.as_str(), self.id)
        } else {
            self.path.display().to_string()
        }
    }
}
