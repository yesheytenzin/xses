//! Write: IR -> each target tool's native session records.
//!
//! One module per target. Writers are additive-only and atomic (temp file ->
//! rename, with a `.bak` of any pre-existing target), and deterministic
//! synthetic ids keep re-imports idempotent.
mod claude;
mod codex;
mod export;
mod opencode;
mod pi;

pub(crate) use codex::codex_state_db_pub;
pub(crate) use export::export_trace;

use crate::canonical::{CanonicalSession, LossReport};
use crate::model::Tool;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What a writer produced for a session: the artifacts recorded in the index
/// (and removed again by `clean`).
pub struct WriteOutcome {
    pub artifacts: Vec<PathBuf>,
}

/// Write one session into one target store. Failures are journaled
/// (`crate::issues`) so `xses doctor` can explain them later.
pub fn session(target: Tool, session: &CanonicalSession) -> (WriteOutcome, LossReport) {
    let (messages, mut report) = session.messages();
    let files = match target {
        Tool::Pi | Tool::Omp => vec![pi::write(target, session, &messages)],
        Tool::Claude => claude::write(session, &messages),
        Tool::Codex => vec![codex::write(session, &messages)],
        Tool::Opencode => vec![opencode::write(session, &messages)],
    };
    if files.iter().all(|f| f.is_none()) {
        report.dropped += session.message_count();
    }
    let artifacts: Vec<PathBuf> = files.into_iter().flatten().collect();
    (WriteOutcome { artifacts }, report)
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
pub(super) fn push(buf: &mut Vec<u8>, v: &Value) {
    buf.extend(serde_json::to_vec(v).unwrap_or_default());
    buf.push(b'\n');
}
pub(super) fn role_str(r: crate::canonical::Role) -> &'static str {
    match r {
        crate::canonical::Role::User => "user",
        crate::canonical::Role::Assistant => "assistant",
        crate::canonical::Role::System => "system",
    }
}
pub(super) fn first_ask(msgs: &[(crate::canonical::Role, String)]) -> String {
    msgs.iter()
        .find(|(r, _)| matches!(r, crate::canonical::Role::User))
        .map(|(_, t)| {
            t.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(120)
                .collect::<String>()
        })
        .unwrap_or_else(|| "imported".into())
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
/// Import artifact id for a source session; import filenames and opencode
/// slugs embed it, so `clean` can map artifacts back to their session.
pub fn import_id_for(tool: Tool, id: &str) -> String {
    import_id(tool, id)
}
pub(super) fn iso(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}
pub(super) fn iso_stamp(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H-%M-%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Block, Role};

    #[test]
    fn flattened_tool_traffic_is_labeled_transcript_not_live() {
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Claude,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            blocks: vec![
                Block::ToolCall {
                    name: "bash".into(),
                    args: "{}".into(),
                },
                Block::ToolResult {
                    name: "bash".into(),
                    output: "ok".into(),
                },
            ],
        };
        let (msgs, report) = session.messages();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].1.contains("transcript, not live"));
        assert!(msgs[1].1.contains("transcript, not live"));
        assert!(!msgs[1].1.starts_with("[tool result:"));
        assert_eq!(report.normalized, 2);
        assert_eq!(report.dropped, 0);
    }

    #[test]
    fn gaps_render_in_place_as_system_markers() {
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Claude,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            blocks: vec![
                Block::Text {
                    role: Role::User,
                    text: "before".into(),
                },
                Block::Gap {
                    kind: "image".into(),
                    detail: "image attachment not carried over".into(),
                },
                Block::Text {
                    role: Role::User,
                    text: "after".into(),
                },
            ],
        };
        let (msgs, report) = session.messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].0, Role::System);
        assert!(msgs[1].1.contains("[gap in imported history:"));
        assert_eq!(report.dropped, 1);
        assert_eq!(session.gap_kinds(), vec!["image".to_string()]);
    }

    #[test]
    fn empty_thinking_flattens_to_nothing() {
        let session = CanonicalSession {
            id: "id".into(),
            tool: Tool::Codex,
            cwd: "/tmp/project".into(),
            created: Utc::now(),
            blocks: vec![
                Block::Text {
                    role: Role::User,
                    text: "hi".into(),
                },
                Block::Thinking { text: "".into() },
            ],
        };
        let (msgs, report) = session.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(report.downgraded, 0);
    }
}
