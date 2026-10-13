//! Write: IR → each target tool's native session records.
//!
//! One module per target. Writers are additive-only and atomic (temp file →
//! rename, with a `.bak` of any pre-existing target), and deterministic
//! synthetic ids keep re-imports idempotent. Structured parts (tool calls and
//! results, reasoning, context, images, patches, usage) are written as the
//! target's own constructs; anything a target cannot hold falls back to
//! labelled text or a named gap, and `xses doctor` reports the difference.
mod claude;
mod codex;
mod export;
mod opencode;
mod pi;

pub(crate) use codex::retire_codex;
pub(crate) use export::export_trace;

use crate::canonical::{Block, CanonicalSession, LossReport, Role};
use crate::model::Tool;
use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Serializes tests that redirect `$HOME`: writers resolve their target
/// directory through `dirs::home_dir()`, which reads the environment.
#[cfg(test)]
pub(super) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mapping version recorded in every import. Bump it whenever the writers
/// change what they emit, so `sync` can re-import and upgrade old copies.
pub(crate) const MAPPING: u32 = 7;

/// Shown as a system message at the top of every import: the transcript that
/// follows already ran somewhere else and must not be treated as live state.
pub(super) const IMPORT_NOTE: &str = "Imported history from another agent's session. \
     The tool calls and results below already executed there; treat them as a transcript to \
     read, never as pending state to continue.";

/// Labelled text for blocks a target cannot hold natively. Every writer
/// renders these identically so a transcript reads the same everywhere.
/// Returns `None` for blocks the writer must handle structurally
/// (text, thinking, tool calls/results, usage, inline images).
pub(super) fn fallback_text(block: &Block) -> Option<String> {
    match block {
        Block::Image {
            mime,
            data: None,
            path,
            bytes,
        } => Some(format!(
            "[image reference: {} ({bytes} bytes)]",
            path.as_ref().map(|p| p.display().to_string())
                .unwrap_or_else(|| mime.clone().unwrap_or_else(|| "image".into()))
        )),
        Block::Context { kind, text, path } => {
            let body = text
                .clone()
                .or_else(|| path.as_ref().map(|p| p.display().to_string()))
                .unwrap_or_default();
            Some(format!("[context: {kind}]\n{body}"))
        }
        Block::Patch { files, .. } => Some(format!(
            "[patch: {} file{}]\n{}",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\n")
        )),
        Block::Compressed { text } => Some(format!("[compaction]\n{text}")),
        Block::Gap { detail, .. } => {
            Some(format!("[gap in imported history: {detail}]"))
        }
        _ => None,
    }
}

/// Token/cost accounting for one message: every writer reads the same shape.
#[derive(Default, Clone)]
pub(super) struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub model: Option<String>,
    pub provider: Option<String>,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.reasoning
    }
}

/// The usage block of a message group, if it recorded one.
pub(super) fn usage_of(blocks: &[&Block]) -> Usage {
    blocks
        .iter()
        .find_map(|b| match b {
            Block::Usage {
                input,
                output,
                reasoning,
                cache_read,
                cache_write,
                cost,
                model,
                provider,
            } => Some(Usage {
                input: *input,
                output: *output,
                reasoning: *reasoning,
                cache_read: *cache_read,
                cache_write: *cache_write,
                cost: *cost,
                model: model.clone(),
                provider: provider.clone(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}

/// Tool arguments as JSON, falling back to the raw string when the source
/// did not record structured args.
pub(super) fn args_json(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!(args))
}

/// Tool arguments as a JSON object (for validators that reject non-objects).
pub(super) fn args_json_object(args: &str) -> Value {
    serde_json::from_str(args)
        .ok()
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({"raw": args}))
}

/// A row timestamp that never goes backwards: the source message's own time
/// when it recorded one, else one millisecond past the previous row.
pub(super) fn next_row_time(
    session: &CanonicalSession,
    msg: u32,
    prev: &mut DateTime<Utc>,
) -> DateTime<Utc> {
    let next = match session.msg_times.get(&msg).copied() {
        Some(ts) if ts > *prev => ts,
        _ => *prev + chrono::Duration::milliseconds(1),
    };
    *prev = next;
    next
}

/// Target-safe id for one side of a call/result pair: the mapped source id
/// when recorded, else a minted value both sides of the pair share.
pub(super) fn fallback_call_id(
    ids: &mut TargetIds,
    call_id: &Option<String>,
    minted: &mut usize,
    name: &str,
) -> String {
    match call_id {
        Some(c) => ids.get(c),
        None => {
            *minted += 1;
            format!("call-{minted}-{name}")
        }
    }
}

/// FNV-1a mix shared by every deterministic id below.
fn hash128(key: &str) -> [u32; 4] {
    let mut h = [0x811c9dc5u32; 4];
    for (i, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[i % 4] ^= b;
        h[i % 4] = h[i % 4].wrapping_mul(0x01000193);
    }
    h
}

fn uuid_shape(h: [u32; 4]) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        h[0],
        h[1] & 0xffff,
        h[2] & 0xfff,
        h[3] & 0xfff,
        (((h[1] as u64) << 32) | h[0] as u64) & 0xffff_ffff_ffff
    )
}

/// Deterministic UUID-shaped id derived from a seed + index (claude's picker
/// requires UUID-shaped leafUuid/parent chains).
pub(super) fn uuid_like(seed: &str, i: usize) -> String {
    uuid_shape(hash128(&format!("{seed}:{i}")))
}

/// Writers use immutable generation IDs. Source provenance remains unchanged
/// in the manifest; only the destination's native identifier includes revision.
pub fn session_revision(
    target: Tool,
    session: &CanonicalSession,
    revision: &str,
) -> Result<(Vec<PathBuf>, LossReport)> {
    let (_, report) = session.transfer_summary();
    let files = match target {
        Tool::Pi | Tool::Omp => vec![pi::write(target, session, Some(revision))],
        Tool::Claude => claude::write(session, Some(revision)),
        Tool::Codex => vec![codex::write(session, Some(revision))],
        Tool::Opencode => vec![opencode::write(session, Some(revision))],
    };
    if files.is_empty() || files.iter().any(Option::is_none) {
        anyhow::bail!(
            "{} import failed; details are recorded by xses doctor",
            target.as_str()
        );
    }
    Ok((files.into_iter().flatten().collect(), report))
}

pub(super) fn artifact_key(session: &CanonicalSession, revision: Option<&str>) -> String {
    match revision {
        Some(revision) => format!("{}@{revision}", session.id),
        None => session.id.clone(),
    }
}

/// Import artifact id for a source session; import filenames and opencode
/// slugs embed it, so `clean` can map artifacts back to their session.
pub fn import_id_for(tool: Tool, id: &str) -> String {
    import_id(tool, id)
}

/// Title every store shows: the source agent's own title when it has one
/// (preserving a provenance prefix an earlier import added), else the first
/// user ask.
pub(super) fn title_for(session: &CanonicalSession) -> String {
    let store = session.tool.as_str();
    let raw = session
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| first_ask(session));
    // `[claude] fix the parser` → agent `claude`, body `fix the parser`.
    let (agent, body) = match raw.strip_prefix('[') {
        Some(rest) => match rest.find(']') {
            Some(end) if Tool::parse(&rest[..end]).is_some() => {
                let body = rest[end + 1..].trim();
                (rest[..end].to_string(), body.to_string())
            }
            _ => (store.to_string(), raw.clone()),
        },
        None => (store.to_string(), raw.clone()),
    };
    let body = if body.is_empty() { "imported" } else { &body };
    format!("[{agent}] {body}")
}

/// First user ask, for the session title every store shows.
pub(super) fn first_ask(session: &CanonicalSession) -> String {
    session
        .blocks()
        .find_map(|b| match b {
            Block::Text {
                role: Role::User,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .map(|t| {
            t.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(120)
                .collect::<String>()
        })
        .unwrap_or_else(|| "imported".into())
}

/// Which role a group of blocks carries, falling back when it has no text.
pub(super) fn group_role(blocks: &[&Block], fallback: Role) -> Role {
    blocks
        .iter()
        .find_map(|b| match b {
            Block::Text { role, .. } => Some(*role),
            _ => None,
        })
        .unwrap_or(fallback)
}

pub(super) fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    // First backup wins: the `.bak` keeps the pre-xses original for human
    // recovery, not a rotation — overwriting it on every re-import would
    // leave only the previous import behind when it is needed most.
    if path.exists() {
        let bak = path.with_extension("bak");
        if !bak.exists() {
            fs::copy(path, &bak)?;
        }
    }
    crate::storage::atomic_write(path, bytes)
}

/// Assigns ids a target will accept, keeping a call and its result on the same
/// value. Anthropic validates `tool_use.id` against `^[A-Za-z0-9_-]+$` (a 400
/// otherwise), and source ids can carry more — codex/omp compose
/// `call_…|fc_…` — while the other stores take any string. One mapping keeps
/// every target consistent, so an id never has to be rewritten per target.
#[derive(Default)]
pub(super) struct TargetIds {
    assigned: std::collections::HashMap<String, String>,
    used: std::collections::HashSet<String>,
}

impl TargetIds {
    /// The target-safe id for a source id (stable within one session).
    pub fn get(&mut self, original: &str) -> String {
        if let Some(existing) = self.assigned.get(original) {
            return existing.clone();
        }
        let base = safe_id(original);
        let mut candidate = base.clone();
        let mut n = 2;
        while !self.used.insert(candidate.clone()) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        self.assigned
            .insert(original.to_string(), candidate.clone());
        candidate
    }
}

/// `call_x|fc_y` keeps the call id itself; anything else outside the allowed
/// class becomes `_`, so the result never depends on a provider's id space.
fn safe_id(id: &str) -> String {
    let head = id.split('|').next().unwrap_or(id);
    let mut out: String = head
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("call");
    }
    out
}

/// Atomic write that journals the reason on failure: writers signal "nothing
/// produced" with `None`, so without this the OS error would vanish and
/// `doctor` could only say "target wrote nothing".
pub(super) fn atomic_write_journaled(path: &Path, bytes: &[u8], subject: &str) -> bool {
    match atomic_write(path, bytes) {
        Ok(()) => true,
        Err(e) => {
            crate::issues::record("write-failed", subject, &format!("{}: {e}", path.display()));
            false
        }
    }
}

/// Journal any non-write failure (serialization, sidecar, payload staging).
pub(super) fn journal_failure(subject: &str, detail: &str) {
    crate::issues::record("write-failed", subject, detail);
}

pub(super) fn push(buf: &mut Vec<u8>, v: &Value) {
    buf.extend(serde_json::to_vec(v).unwrap_or_default());
    buf.push(b'\n');
}

/// Deterministic synthetic session id (idempotent re-imports).
pub(super) fn synth_id(tool: Tool, id: &str) -> String {
    uuid_shape(hash128(&format!("{}:{}", tool.as_str(), id)))
}

pub(super) fn import_id(tool: Tool, id: &str) -> String {
    crate::model::slug_id(&synth_id(tool, id), 32)
}

pub(super) fn iso(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(super) fn iso_stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// Header manifest: what this import is, and how to explain its losses.
pub(super) fn manifest(session: &CanonicalSession) -> Value {
    let kinds = session.kind_counts();
    let mut blocks = serde_json::Map::new();
    for (kind, n) in kinds {
        blocks.insert(kind.to_string(), Value::from(n));
    }
    let gaps: Vec<String> = session.gap_kinds();
    serde_json::json!({
        "source": session.tool.as_str(),
        "srcId": session.id,
        "mapping": MAPPING,
        "blocks": Value::Object(blocks),
        "gaps": gaps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::Event;

    fn session(events: Vec<Event>) -> CanonicalSession {
        CanonicalSession {
            id: "id".into(),
            tool: Tool::Claude,
            cwd: "/tmp/project".into(),
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
    fn titles_use_the_source_title_and_keep_provenance() {
        let mut s = session(vec![ev(
            0,
            Block::Text {
                role: Role::User,
                text: "a long first ask".into(),
            },
        )]);
        // no source title → first ask, prefixed with the store it came from
        s.title = None;
        assert_eq!(title_for(&s), "[claude] a long first ask");
        // source title wins over the first ask
        s.title = Some("Fix the parser".into());
        assert_eq!(title_for(&s), "[claude] Fix the parser");
        // an imported source keeps the agent it originally came from
        s.title = Some("[omp] Fix the parser".into());
        assert_eq!(title_for(&s), "[omp] Fix the parser");
        // empty titles fall back
        s.title = Some("  ".into());
        assert_eq!(title_for(&s), "[claude] a long first ask");
    }

    #[test]
    fn manifest_counts_kinds_and_gaps() {
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
                    text: "why".into(),
                    signature: None,
                },
            ),
            ev(
                1,
                Block::ToolCall {
                    name: "bash".into(),
                    args: "{}".into(),
                    call_id: Some("c".into()),
                    status: None,
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
        let m = manifest(&s);
        assert_eq!(m["mapping"], MAPPING);
        assert_eq!(m["blocks"]["tool_call"], 1);
        assert_eq!(m["gaps"][0], "image");
    }
}
