//! Intermediate representation: Extract → Transform → Build.
//!
//! Every source transcript parses into a `CanonicalSession` (a list of typed
//! events); writers serialize that into a target tool's native schema. The IR
//! carries loss accounting per block, so `convert --dry-run` can report exactly
//! what survives a conversion.
use crate::model::Tool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// What happened to a source block during extraction/translation.
/// Mirrors memorph's MappingDisposition taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Block converted with full semantic content.
    Preserved,
    /// Block kept but reformatted (e.g. tool call flattened to text).
    Normalized,
    /// Semantics weakened (e.g. thinking → text, structure lost).
    Downgraded,
    /// Dropped entirely; counted in the loss report.
    Dropped,
    /// Target format cannot represent this block at all.
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        role: Role,
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        output: String,
    },
    /// Codex-style compaction summary injected mid-conversation.
    Compressed {
        text: String,
    },
}

impl Block {
    /// (text destined for flattening, fidelity) for targets that only carry text.
    pub fn flatten(&self) -> Option<(String, Fidelity)> {
        match self {
            Block::Text { text, .. } => Some((text.clone(), Fidelity::Preserved)),
            Block::Thinking { text } => Some((format!("[thinking]\n{text}"), Fidelity::Downgraded)),
            Block::ToolCall { name, args } => {
                Some((format!("[tool call: {name}]\n{args}"), Fidelity::Normalized))
            }
            Block::ToolResult { name, output } => Some((
                format!("[tool result: {name}]\n{output}"),
                Fidelity::Normalized,
            )),
            Block::Compressed { text } => {
                Some((format!("[compaction]\n{text}"), Fidelity::Normalized))
            }
        }
    }
}

/// Tool-agnostic session: the conversion hub. Mirrors unisessions' `NativeSession
/// + TextMessage` pair, with typed events instead of strings.
#[derive(Debug, Clone)]
pub struct CanonicalSession {
    pub id: String,
    pub tool: Tool,
    pub cwd: std::path::PathBuf,
    pub created: DateTime<Utc>,
    pub blocks: Vec<Block>,
}

impl CanonicalSession {
    /// Text-only view for tools whose format carries only messages.
    /// Returns flattened messages plus a fidelity tally.
    pub fn messages(&self) -> (Vec<(Role, String)>, LossReport) {
        let mut msgs = Vec::new();
        let mut report = LossReport::default();
        for b in &self.blocks {
            if let Some((text, f)) = b.flatten() {
                let role = match b {
                    Block::Text { role, .. } => *role,
                    _ => Role::Assistant,
                };
                if !text.trim().is_empty() {
                    msgs.push((role, text));
                }
                report.tally(f);
            }
        }
        (msgs, report)
    }

    pub fn message_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|b| matches!(b, Block::Text { .. }))
            .count()
    }
}

/// Per-session accounting of what fidelity survived conversion.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct LossReport {
    pub preserved: usize,
    pub normalized: usize,
    pub downgraded: usize,
    pub dropped: usize,
    pub unsupported: usize,
}

impl LossReport {
    pub fn tally(&mut self, f: Fidelity) {
        match f {
            Fidelity::Preserved => self.preserved += 1,
            Fidelity::Normalized => self.normalized += 1,
            Fidelity::Downgraded => self.downgraded += 1,
            Fidelity::Dropped => self.dropped += 1,
            Fidelity::Unsupported => self.unsupported += 1,
        }
    }

    pub fn merge(&mut self, o: LossReport) {
        self.preserved += o.preserved;
        self.normalized += o.normalized;
        self.downgraded += o.downgraded;
        self.dropped += o.dropped;
        self.unsupported += o.unsupported;
    }

    pub fn total_lost(&self) -> usize {
        self.dropped + self.unsupported
    }
}
