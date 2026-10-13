//! Intermediate representation: Extract → Transform → Build.
//!
//! Every source transcript parses into a `CanonicalSession`: a flat stream of
//! typed [`Event`]s, each carrying the source message it belonged to (`msg`)
//! so writers can rebuild native message grouping. The IR keeps everything the
//! sources hold — tool calls with their ids, tool outputs with error state,
//! reasoning, injected context, images, patches, usage — and records per-block
//! fidelity so previews can report exactly what a conversion costs.
use crate::model::Tool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// One typed block of a transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        role: Role,
        text: String,
    },
    /// Reasoning. `signature` is the provider's signed blob when the source
    /// kept one; it is never replayable elsewhere, so writers carry the text.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        name: String,
        args: String,
        /// Provider call id, when the source exposes one (used to pair results).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    ToolResult {
        name: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Injected context: system reminders, AGENTS.md, environment snapshots,
    /// attachments referenced by path, subagent/subtask notes.
    Context {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// Inline binary payload (images). `data` is base64 as the sources store it.
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        #[serde(default)]
        bytes: u64,
    },
    /// A recorded code change.
    Patch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        files: Vec<PathBuf>,
    },
    /// Compaction summary injected mid-conversation.
    Compressed {
        text: String,
    },
    /// Token/cost accounting for the message that preceded it.
    Usage {
        #[serde(default)]
        input: u64,
        #[serde(default)]
        output: u64,
        #[serde(default)]
        reasoning: u64,
        #[serde(default)]
        cache_read: u64,
        #[serde(default)]
        cache_write: u64,
        #[serde(default)]
        cost: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Content the source carried that no transcript can reproduce: a redacted
    /// turn, an unreadable record, a tool output that was never recorded.
    /// Renders as its own message so resuming agents see the hole in context.
    Gap {
        /// Stable machine key (`image`, `thinking`, `tool-output`, …).
        kind: String,
        /// One line for the transcript, e.g. `image attachment (2.1 MB png)`.
        detail: String,
    },
}

/// A block plus the source message it belongs to, so writers can regroup
/// multi-part messages (assistant text + thinking + tool calls) natively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub msg: u32,
    pub block: Block,
}

/// Builder handed to extractors: tracks message boundaries and keeps the
/// event stream in transcript order, with the source's own timestamp per
/// message when the store records one.
#[derive(Debug, Default)]
pub struct Events {
    list: Vec<Event>,
    msg: u32,
    /// Source message index -> the time the source itself recorded.
    times: std::collections::BTreeMap<u32, DateTime<Utc>>,
}

impl Events {
    /// Start a new source message.
    pub fn boundary(&mut self) {
        self.msg += 1;
    }

    /// Record the source's timestamp for the message being built. Called after
    /// `boundary()`, so it lands on that message.
    pub fn at(&mut self, ts: DateTime<Utc>) {
        self.times.insert(self.msg, ts);
    }

    pub fn push(&mut self, block: Block) {
        self.list.push(Event {
            msg: self.msg,
            block,
        });
    }

    #[cfg(test)]
    pub fn into_vec(self) -> Vec<Event> {
        self.list
    }

    /// Events plus the per-message timestamps the source recorded.
    pub fn into_parts(self) -> (Vec<Event>, std::collections::BTreeMap<u32, DateTime<Utc>>) {
        (self.list, self.times)
    }
}

impl Block {
    /// Stable machine key for accounting and gap reporting.
    pub fn kind(&self) -> &'static str {
        match self {
            Block::Text { .. } => "text",
            Block::Thinking { .. } => "thinking",
            Block::ToolCall { .. } => "tool_call",
            Block::ToolResult { .. } => "tool_result",
            Block::Context { .. } => "context",
            Block::Image { .. } => "image",
            Block::Patch { .. } => "patch",
            Block::Compressed { .. } => "compaction",
            Block::Usage { .. } => "usage",
            Block::Gap { .. } => "gap",
        }
    }

    /// (text destined for flattening, fidelity) for targets that carry only
    /// text — trace exports, and the fallback when a construct is unusable.
    ///
    /// Flattened tool traffic is labeled as a transcript of a past run: the
    /// target agent must not mistake it for live tool state it can continue.
    pub fn flatten(&self) -> Option<(String, Fidelity)> {
        match self {
            Block::Text { text, .. } => Some((text.clone(), Fidelity::Preserved)),
            Block::Thinking { text, .. } => {
                if text.trim().is_empty() {
                    return None;
                }
                Some((
                    format!("[previous thinking — transcript, not live]\n{text}"),
                    Fidelity::Downgraded,
                ))
            }
            Block::ToolCall { name, args, .. } => Some((
                format!("[previous tool call: {name} — transcript, not live]\n{args}"),
                Fidelity::Normalized,
            )),
            Block::ToolResult { name, output, .. } => Some((
                format!("[previous tool result: {name} — transcript, not live]\n{output}"),
                Fidelity::Normalized,
            )),
            Block::Context { kind, text, path } => {
                let body = match (text, path) {
                    (Some(t), _) => t.clone(),
                    (None, Some(p)) => p.display().to_string(),
                    (None, None) => String::new(),
                };
                Some((format!("[context: {kind}]\n{body}"), Fidelity::Normalized))
            }
            Block::Image {
                mime, bytes, path, ..
            } => {
                let what = match (mime.as_deref(), path.as_deref()) {
                    (Some(m), _) => m.to_string(),
                    (None, Some(p)) => p.display().to_string(),
                    (None, None) => "image".into(),
                };
                Some((
                    format!("[image: {what} ({bytes} bytes) — not carried in text]"),
                    Fidelity::Downgraded,
                ))
            }
            Block::Patch { files, .. } => Some((
                format!(
                    "[patch: {} file{}]\n{}",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    files
                        .iter()
                        .map(|f| f.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                Fidelity::Normalized,
            )),
            Block::Compressed { text } => {
                Some((format!("[compaction]\n{text}"), Fidelity::Normalized))
            }
            // Accounting rides along with the message; it has no text of its own.
            Block::Usage { .. } => None,
            Block::Gap { detail, .. } => Some((
                format!("[gap in imported history: {detail}]"),
                Fidelity::Dropped,
            )),
        }
    }
}

/// Tool-agnostic session: the conversion hub.
#[derive(Debug, Clone)]
pub struct CanonicalSession {
    pub id: String,
    pub tool: Tool,
    pub cwd: PathBuf,
    pub created: DateTime<Utc>,
    /// The source agent's own title for this session when it records one
    /// (pi/omp `title`, opencode `session.title`, …), possibly carrying an
    /// `[agent]` prefix from an earlier import. Titles derived from the first
    /// ask collide whenever several sessions share that ask.
    pub title: Option<String>,
    pub events: Vec<Event>,
    /// Source message index -> the timestamp the source recorded, when it
    /// records one. Writers carry these so an import keeps the session's real
    /// timeline instead of one synthetic time for every row.
    pub msg_times: std::collections::BTreeMap<u32, DateTime<Utc>>,
}

impl CanonicalSession {
    /// Text-only view (trace exports and text fallbacks): flattened messages
    /// plus a fidelity tally. Gaps render in place so the hole stays visible.
    pub fn messages(&self) -> (Vec<(Role, String)>, LossReport) {
        let mut msgs = Vec::new();
        let mut report = LossReport::default();
        for e in &self.events {
            if let Some((text, f)) = e.block.flatten() {
                let role = match &e.block {
                    Block::Text { role, .. } => *role,
                    Block::Gap { .. } => Role::System,
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

    /// Blocks grouped by their source message, in order — the shape writers
    /// need to emit multi-part native messages.
    pub fn grouped(&self) -> Vec<(u32, Vec<&Block>)> {
        let mut out: Vec<(u32, Vec<&Block>)> = Vec::new();
        for e in &self.events {
            match out.last_mut() {
                Some((msg, blocks)) if *msg == e.msg => blocks.push(&e.block),
                _ => out.push((e.msg, vec![&e.block])),
            }
        }
        out
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.events.iter().map(|e| &e.block)
    }

    pub fn message_count(&self) -> usize {
        self.blocks()
            .filter(|b| matches!(b, Block::Text { .. }))
            .count()
    }

    /// Kinds of content lost in this session (gap keys, in first-seen order).
    pub fn gap_kinds(&self) -> Vec<String> {
        let mut kinds = Vec::new();
        for b in self.blocks() {
            if let Block::Gap { kind, .. } = b {
                if !kinds.contains(kind) {
                    kinds.push(kind.clone());
                }
            }
        }
        kinds
    }

    /// Per-kind block counts, for the import manifest and `doctor`.
    pub fn kind_counts(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for b in self.blocks() {
            let kind = b.kind();
            match out.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => out.push((kind, 1)),
            }
        }
        out
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

/// Apply `--scope last:N`: keep only the trailing N user turns (plus
/// everything after the Nth-from-last user message).
pub fn scope_last_n(session: CanonicalSession, n: usize) -> CanonicalSession {
    let users: Vec<usize> = session
        .events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match &e.block {
            Block::Text {
                role: Role::User, ..
            } => Some(i),
            _ => None,
        })
        .collect();
    let keep_from = if n == 0 || users.is_empty() {
        None
    } else if n >= users.len() {
        return session;
    } else {
        Some(users[users.len() - n])
    };
    let events = match keep_from {
        Some(start) => session.events[start..].to_vec(),
        None => Vec::new(),
    };
    CanonicalSession { events, ..session }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(events: Vec<Event>) -> CanonicalSession {
        CanonicalSession {
            id: "id".into(),
            tool: Tool::Pi,
            cwd: "/tmp".into(),
            created: Utc::now(),
            title: None,
            events,
            msg_times: Default::default(),
        }
    }

    fn ev(msg: u32, block: Block) -> Event {
        Event { msg, block }
    }

    #[test]
    fn grouping_follows_source_messages() {
        let s = session(vec![
            ev(
                0,
                Block::Text {
                    role: Role::User,
                    text: "hi".into(),
                },
            ),
            ev(
                1,
                Block::Thinking {
                    text: "hmm".into(),
                    signature: None,
                },
            ),
            ev(
                1,
                Block::Text {
                    role: Role::Assistant,
                    text: "ok".into(),
                },
            ),
            ev(
                1,
                Block::ToolCall {
                    name: "bash".into(),
                    args: "{}".into(),
                    call_id: Some("c1".into()),
                    status: None,
                },
            ),
        ]);
        let groups = s.grouped();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].1.len(), 3);
    }

    #[test]
    fn usage_has_no_text_and_kinds_are_counted() {
        let s = session(vec![
            ev(
                0,
                Block::Text {
                    role: Role::User,
                    text: "hi".into(),
                },
            ),
            ev(
                1,
                Block::Usage {
                    input: 5,
                    output: 6,
                    reasoning: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cost: 0.0,
                    model: None,
                },
            ),
            ev(
                1,
                Block::Gap {
                    kind: "image".into(),
                    detail: "gone".into(),
                },
            ),
        ]);
        let (msgs, report) = s.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(report.dropped, 1);
        let kinds = s.kind_counts();
        assert!(kinds.contains(&("usage", 1)));
        assert!(kinds.contains(&("gap", 1)));
    }

    #[test]
    fn scope_keeps_trailing_turns_with_their_parts() {
        let s = session(vec![
            ev(
                0,
                Block::Text {
                    role: Role::User,
                    text: "one".into(),
                },
            ),
            ev(
                1,
                Block::Text {
                    role: Role::Assistant,
                    text: "two".into(),
                },
            ),
            ev(
                2,
                Block::Text {
                    role: Role::User,
                    text: "three".into(),
                },
            ),
            ev(
                3,
                Block::Thinking {
                    text: "t".into(),
                    signature: None,
                },
            ),
            ev(
                3,
                Block::Text {
                    role: Role::Assistant,
                    text: "four".into(),
                },
            ),
        ]);
        let scoped = scope_last_n(s, 1);
        assert_eq!(scoped.events.len(), 3);
        assert!(matches!(&scoped.events[1].block, Block::Thinking { .. }));
    }
}
