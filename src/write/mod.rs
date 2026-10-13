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

pub(crate) use codex::codex_state_db_pub;
pub(crate) use export::export_trace;

use crate::canonical::{Block, CanonicalSession, LossReport, Role};
use crate::model::Tool;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Serializes tests that redirect `$HOME`: writers resolve their target
/// directory through `dirs::home_dir()`, which reads the environment.
#[cfg(test)]
pub(super) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mapping version recorded in every import. Bump it whenever the writers
/// change what they emit, so `sync` can re-import and upgrade old copies.
pub(crate) const MAPPING: u32 = 4;

/// What a writer produced for a session: the artifacts recorded in the index
/// (and removed again by `clean`).
pub struct WriteOutcome {
    pub artifacts: Vec<PathBuf>,
}

/// Write one session into one target store. Failures are journaled
/// (`crate::issues`) so `xses doctor` can explain them later.
pub fn session(target: Tool, session: &CanonicalSession) -> (WriteOutcome, LossReport) {
    let mut report = LossReport::default();
    for block in session.blocks() {
        if let Some((_, fidelity)) = block.flatten() {
            report.tally(fidelity);
        }
    }
    let files = match target {
        Tool::Pi | Tool::Omp => vec![pi::write(target, session)],
        Tool::Claude => claude::write(session),
        Tool::Codex => vec![codex::write(session)],
        Tool::Opencode => vec![opencode::write(session)],
    };
    if files.iter().all(|f| f.is_none()) {
        report.dropped += session.message_count();
    }
    let artifacts: Vec<PathBuf> = files.into_iter().flatten().collect();
    (WriteOutcome { artifacts }, report)
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
    let parent = path.parent().context("no parent")?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        let bak = path.with_extension("bak");
        if !bak.exists() {
            fs::copy(path, &bak)?;
        }
    }
    let tmp = parent.join(format!(
        ".xses-{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Ok(dir) = File::open(parent) {
        dir.sync_all().ok();
    }
    Ok(())
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
    let mut h = [0x811c9dc5u32; 4];
    let key = format!("{}:{}", tool.as_str(), id);
    for (i, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[i % 4] ^= b;
        h[i % 4] = h[i % 4].wrapping_mul(0x01000193);
    }
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        h[0],
        h[1] & 0xffff,
        h[2] & 0xfff,
        h[3] & 0xfff,
        (((h[1] as u64) << 32) | h[0] as u64) & 0xffff_ffff_ffff
    )
}

pub(super) fn import_id(tool: Tool, id: &str) -> String {
    synth_id(tool, id)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect()
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
    use crate::canonical::{Event, Fidelity};

    fn session(events: Vec<Event>) -> CanonicalSession {
        CanonicalSession {
            id: "id".into(),
            tool: Tool::Claude,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            title: None,
            events,
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

    #[test]
    fn text_flattening_still_labels_history() {
        let s = session(vec![ev(
            0,
            Block::ToolResult {
                name: "bash".into(),
                output: "ok".into(),
                call_id: None,
                is_error: false,
                duration_ms: None,
            },
        )]);
        let (msgs, report) = s.messages();
        assert!(msgs[0].1.contains("transcript, not live"));
        assert_eq!(report.normalized, 1);
    }

    #[test]
    fn gaps_render_in_place_as_system_markers() {
        let s = session(vec![
            ev(
                0,
                Block::Text {
                    role: Role::User,
                    text: "before".into(),
                },
            ),
            ev(
                1,
                Block::Gap {
                    kind: "image".into(),
                    detail: "image attachment not carried over".into(),
                },
            ),
            ev(
                2,
                Block::Text {
                    role: Role::User,
                    text: "after".into(),
                },
            ),
        ]);
        let (msgs, report) = s.messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].0, Role::System);
        assert!(msgs[1].1.contains("[gap in imported history:"));
        assert_eq!(report.dropped, 1);
        assert_eq!(s.gap_kinds(), vec!["image".to_string()]);
    }

    #[test]
    fn empty_thinking_flattens_to_nothing() {
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
                    text: "".into(),
                    signature: None,
                },
            ),
        ]);
        let (msgs, report) = s.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(report.downgraded, 0);
    }

    #[test]
    fn fidelity_tally_counts_each_block() {
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
                Block::Image {
                    mime: Some("image/png".into()),
                    data: Some("qq==".into()),
                    path: None,
                    bytes: 3,
                },
            ),
        ]);
        let (_, report) = s.messages();
        assert_eq!(report.preserved, 1);
        assert_eq!(report.downgraded, 1);
        let _ = Fidelity::Preserved;
    }
}
