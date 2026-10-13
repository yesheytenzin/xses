//! claude extractor: JSONL transcript rows.
use super::common::{
    any_time, attachment_content, collect_parts, lines, num, parse_record,
    restored_persisted_output,
};
use super::text_of;
use crate::canonical::{Block, Block::*, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) fn claude(path: &Path) -> Result<Events> {
    let mut out = Events::default();
    // Dedupes repeated meta injections (claude re-sends the same skill body on
    // every turn it applies to).
    let mut seen_meta: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (line_no, line) in lines(path)?.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(v) = parse_record(line_no, &line, &mut out) else {
            continue;
        };
        let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Sidechain (subagent) rows are a different conversation: carrying
        // them would present someone else's turns as this transcript's, so
        // they stay in the source.
        if v.get("isSidechain")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        // Meta rows are injections claude made itself (skill bodies, local
        // command output). They are context the model read, so they transfer
        // as context — once: the same injection repeats on every later turn,
        // and one copy is the whole information.
        if v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false) {
            if let Some(text) = v.pointer("/message/content").and_then(text_of) {
                let text = text.trim();
                if !text.is_empty() && seen_meta.insert(text.to_string()) {
                    out.boundary();
                    if let Some(ts) = any_time(&v) {
                        out.at(ts);
                    }
                    out.push(Context {
                        kind: "meta".into(),
                        text: Some(text.to_string()),
                        path: None,
                    });
                }
            }
            continue;
        }
        match t {
            // A compact summary rides as a user row; it is the compaction, not
            // something the user typed.
            "user"
                if v.get("isCompactSummary")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false) =>
            {
                out.boundary();
                if let Some(ts) = any_time(&v) {
                    out.at(ts);
                }
                let summary = v
                    .pointer("/message/content")
                    .and_then(text_of)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "earlier history summarized away".into());
                out.push(Compressed {
                    text: format!("session was compacted; summary follows\n{summary}"),
                });
            }
            "user" | "assistant" | "system" => {
                // claude truncates a big tool output in the transcript and keeps
                // the full text in `<session>/tool-results/*.txt`, pointing at it
                // from `toolUseResult`. The full output is on disk, so the
                // result the model could read is what transfers.
                if let Some(restored) = restored_persisted_output(&v) {
                    out.boundary();
                    if let Some(ts) = any_time(&v) {
                        out.at(ts);
                    }
                    out.push(restored);
                    continue;
                }
                let role = match t {
                    "user" => Role::User,
                    "system" => Role::System,
                    _ => Role::Assistant,
                };
                out.boundary();
                if let Some(ts) = any_time(&v) {
                    out.at(ts);
                }
                if let Some(content) = v.pointer("/message/content") {
                    collect_parts(content, role, Tool::Claude, &mut out);
                }
                if let Some(u) = v.pointer("/message/usage") {
                    out.push(usage_from_claude(u));
                }
            }
            "attachment" => {
                if let Some(block) = claude_attachment(v.get("attachment")) {
                    out.push(block);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

pub(super) fn usage_from_claude(u: &Value) -> Block {
    Usage {
        input: num(u, "/input_tokens"),
        output: num(u, "/output_tokens"),
        reasoning: num(u, "/output_tokens_details/thinking_tokens"),
        cache_read: num(u, "/cache_read_input_tokens"),
        cache_write: num(u, "/cache_creation_input_tokens"),
        cost: 0.0,
        model: None,
        provider: None,
    }
}

/// Content-bearing claude attachment → context block; environment noise is
/// skipped (it is regenerated by every agent on resume).
pub(super) fn claude_attachment(a: Option<&Value>) -> Option<Block> {
    let a = a?;
    let path = a
        .get("filename")
        .and_then(|f| f.as_str())
        .map(PathBuf::from);
    match a.get("type").and_then(|t| t.as_str()) {
        // claude embeds what it read: text (with the line range) or an image
        // payload. Carrying that beats a note that says it was not carried.
        Some("file") => Some(attachment_content(a, path)),
        Some("compact_file_reference") => Some(if a.get("content").is_some() {
            // Embedded content is evidence: carry it whatever the reference is.
            attachment_content(a, path.clone())
        } else {
            match attachment_content(a, path.clone()) {
                // A readable image on disk still transfers.
                image @ Image { .. } => image,
                // Otherwise the reference is all there is: say what it means,
                // so the resumed agent knows it can read the file itself.
                _ => Context {
                    kind: "pre-compaction-reference".into(),
                    text: Some("read before the last compaction; re-read if needed".into()),
                    path,
                },
            }
        }),
        Some("thinking_stripped") => Some(Gap {
            kind: "thinking".into(),
            detail: "reasoning stripped before import; not carried over".into(),
        }),
        Some("queued_command") => {
            let status = a
                .get("prompt")
                .and_then(|p| p.as_str())
                .and_then(|p| p.split("<status>").nth(1)?.split("</status>").next())
                .unwrap_or("event");
            Some(Context {
                kind: "background-task".into(),
                text: Some(format!("background task {status}; output not carried over")),
                path: None,
            })
        }
        _ => None,
    }
}
