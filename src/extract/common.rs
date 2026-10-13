//! Shared record helpers for the source extractors: timestamps, JSONL
//! plumbing, image/blob handling, and the generic part collector.
use super::text_of;
use crate::canonical::{Block, Block::*, Events, Role};
use crate::model::Tool;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Largest binary payload carried inline. Sources embed base64 images; past
/// this size a sync would bloat every target store, so the image is recorded
/// as a gap and the manifest says why.
const MAX_INLINE_BYTES: u64 = 5 * 1024 * 1024;

/// A source row's own timestamp, when it records one.
/// Whatever time a record carries: an RFC3339 string or epoch millis.
pub(super) fn any_time(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(ts) = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
    {
        return Some(ts);
    }
    // opencode keeps milliseconds since the epoch on the message, not a string.
    let ms = v
        .pointer("/time/created")
        .or_else(|| v.get("time_created"))
        .and_then(|t| t.as_i64())?;
    chrono::DateTime::from_timestamp_millis(ms)
}

pub(super) fn num(v: &Value, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(|n| n.as_u64()).unwrap_or(0)
}

pub(super) fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    Ok(BufReader::new(fs::File::open(path)?).lines())
}

/// Parse one JSONL record. A truncated or corrupt line (partial write from a
/// crashed agent) becomes a visible gap instead of failing the whole session.
pub(super) fn parse_record(line_no: usize, line: &str, out: &mut Events) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => Some(v),
        Err(_) => {
            out.push(Gap {
                kind: "unreadable-record".into(),
                detail: format!("record at line {} could not be read", line_no + 1),
            });
            None
        }
    }
}

pub(super) fn content_has_image(content: &Value) -> bool {
    content.as_array().is_some_and(|items| {
        items.iter().any(|it| {
            matches!(
                it.get("type").and_then(|t| t.as_str()),
                Some("image" | "input_image")
            )
        })
    })
}

/// A claude tool result whose text claude stored beside the transcript.
pub(super) fn restored_persisted_output(row: &Value) -> Option<Block> {
    let tur = row.get("toolUseResult")?;
    let path = PathBuf::from(tur.get("persistedOutputPath")?.as_str()?);
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INLINE_BYTES {
        return None; // keep the transcript's own preview text instead
    }
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return None, // not text, or unreadable: leave the preview
    };
    // Context, not a tool result: the transcript row is a plain user string
    // (claude never paired it with a call id), and a `tool_result` whose id
    // matches no `tool_use` is exactly what claude's own resume path rejects.
    Some(Context {
        kind: "tool-output".into(),
        text: Some(format!(
            "[full output restored from claude's tool-results sidecar: {} bytes]\n{body}",
            meta.len()
        )),
        path: Some(path),
    })
}

/// codex's compaction row → `Compressed`. Its `message` is usually empty, so
/// the retained history's size is what tells the reader how much went away.
pub(super) fn codex_compaction(payload: &Value) -> Block {
    let text = payload
        .get("message")
        .and_then(|m| m.as_str())
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let retained = payload
                .get("replacement_history")
                .and_then(|h| h.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            format!("session was compacted; {retained} retained items summarized away")
        });
    Compressed { text }
}

/// What an attachment holds: embedded text or image, an on-disk image, or a
/// note saying precisely what was dropped.
pub(super) fn attachment_content(a: &Value, path: Option<PathBuf>) -> Block {
    let display = a
        .get("displayPath")
        .and_then(|d| d.as_str())
        .map(str::to_string)
        .or_else(|| {
            a.get("filename")
                .and_then(|f| f.as_str())
                .map(|f| f.rsplit('/').next().unwrap_or(f).to_string())
        })
        .unwrap_or_else(|| "file".into());
    let embedded = a.get("content");
    // An image payload: same shape the transcript uses elsewhere.
    if let Some(content) = embedded {
        let kind = content.get("type").and_then(|t| t.as_str());
        if kind == Some("image") || content.get("source").is_some() {
            return image_block(content, Tool::Claude);
        }
    }
    // Text payload: `{type: text, file: {content, startLine, totalLines}}`.
    let file = embedded.and_then(|c| c.get("file"));
    let text = file
        .and_then(|f| f.get("content"))
        .and_then(|c| c.as_str())
        .filter(|t| !t.trim().is_empty());
    if let Some(text) = text {
        let start = file
            .and_then(|f| f.get("startLine"))
            .and_then(|l| l.as_u64());
        let total = file
            .and_then(|f| f.get("totalLines"))
            .and_then(|l| l.as_u64());
        let range = match (start, total) {
            (Some(s), Some(t)) if s > 1 => format!(" (lines {s}-{t})"),
            (_, Some(t)) => format!(" ({t} lines)"),
            _ => String::new(),
        };
        return Context {
            kind: "attachment".into(),
            text: Some(format!("attached {display}{range}\n{text}")),
            path,
        };
    }
    // Nothing embedded: an image on disk still transfers, other files do not.
    let Some(path) = path else {
        return Context {
            kind: "attachment".into(),
            text: Some(format!("attached {display} not carried")),
            path: None,
        };
    };
    let mime = mime_for_path(&path);
    match (mime, fs::metadata(&path)) {
        (Some(mime), Ok(meta)) if meta.is_file() && meta.len() <= MAX_INLINE_BYTES => {
            match fs::read(&path) {
                Ok(bytes) => Image {
                    mime: Some(mime.into()),
                    data: Some(base64_encode(&bytes)),
                    path: Some(path),
                    bytes: meta.len(),
                },
                Err(_) => Context {
                    kind: "attachment".into(),
                    text: Some(format!("attached {display} not carried (unreadable)")),
                    path: Some(path),
                },
            }
        }
        (Some(_), Ok(meta)) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached image {display} not carried ({:.1} MB exceeds the {} MB inline cap)",
                meta.len() as f64 / 1e6,
                MAX_INLINE_BYTES / 1024 / 1024
            )),
            path: Some(path),
        },
        (Some(_), Err(_)) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached image {display} not carried (file is gone)"
            )),
            path: Some(path),
        },
        (None, _) => Context {
            kind: "attachment".into(),
            text: Some(format!(
                "attached file {display} not carried (only images transfer by path)"
            )),
            path: Some(path),
        },
    }
}

/// Image mime for a path, by extension: the only files xses reads from disk.
pub(super) fn mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Image part → `Image` block when it fits the inline cap, otherwise a gap.
pub(super) fn image_block(part: &Value, tool: Tool) -> Block {
    // claude nests the payload one level down:
    // `{type: image, source: {type: base64, media_type, data}}`.
    let part = part
        .get("source")
        .filter(|source| source.is_object())
        .unwrap_or(part);
    let mut mime = part
        .get("mimeType")
        .or_else(|| part.get("mime_type"))
        .or_else(|| part.get("media_type"))
        // opencode `file` parts name it `mime`
        .or_else(|| part.get("mime"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    // pi/omp: {type:image, data, mimeType}; codex: {type:input_image, image_url:"data:…;base64,…"}.
    // `data` is base64, but it can also be a reference (`blob:sha256:<hash>`,
    // `file:`/path) — those blobs live in the agent's own store and are read
    // back so the image transfers for real.
    let raw = part.get("data").and_then(|d| d.as_str());
    let (data, path, bytes) = match raw {
        Some(r) if r.starts_with("blob:") => match resolve_blob(tool, r) {
            Some((b64, n)) => (Some(b64), None, n),
            None => (None, Some(PathBuf::from(r)), 0),
        },
        Some(r) if r.starts_with("file:") || r.starts_with('/') || r.contains("://") => {
            (None, Some(PathBuf::from(r)), 0)
        }
        Some(r) => {
            let bytes = r.len() as u64 * 3 / 4;
            (Some(r.to_string()), None, bytes)
        }
        // codex says `image_url`; opencode's `file` parts say `url`.
        None => match part
            .get("image_url")
            .or_else(|| part.get("url"))
            .and_then(|u| u.as_str())
        {
            Some(url) => match url.split_once("base64,") {
                Some((head, b64)) => {
                    if mime.is_none() {
                        mime = head
                            .trim_start_matches("data:")
                            .split(';')
                            .next()
                            .filter(|m| !m.is_empty())
                            .map(str::to_string);
                    }
                    (Some(b64.to_string()), None, b64.len() as u64 * 3 / 4)
                }
                None => (None, Some(PathBuf::from(url)), 0),
            },
            None => (None, None, 0),
        },
    };
    if bytes > MAX_INLINE_BYTES {
        return Gap {
            kind: "image".into(),
            detail: format!(
                "image ({:.1} MB{}) exceeds the {} MB inline cap",
                bytes as f64 / 1e6,
                mime.as_deref().map(|m| format!(" {m}")).unwrap_or_default(),
                MAX_INLINE_BYTES / 1024 / 1024
            ),
        };
    }
    let path = path.or_else(|| part.get("path").and_then(|p| p.as_str()).map(PathBuf::from));
    Image {
        mime,
        data,
        path,
        bytes,
    }
}

/// Resolve a content-addressed reference (`blob:sha256:<hash>`) against the
/// agent's own blob store, returning base64 of the bytes.
pub(super) fn resolve_blob(tool: Tool, reference: &str) -> Option<(String, u64)> {
    let hash = reference.rsplit(':').next()?;
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let home = dirs::home_dir()?;
    // Content-addressed: the hash identifies the bytes, and the store that
    // holds them can belong to the sibling agent (pi sessions referencing omp
    // blobs, and vice versa), so both are searched.
    let pi = home.join(".pi/agent/blobs");
    let omp = home.join(".omp/agent/blobs");
    let dirs = match tool {
        Tool::Pi => vec![pi, omp],
        _ => vec![omp, pi],
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(hash) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_INLINE_BYTES {
                return None;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            return Some((base64_encode(&bytes), bytes.len() as u64));
        }
    }
    None
}

/// Minimal base64 encoder (no dependency for one call site).
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A tool call whose result never landed in the source store.
pub(super) fn tool_output_gap(name: &str) -> Block {
    Gap {
        kind: "tool-output".into(),
        detail: format!("{name} ran but its output was not recorded"),
    }
}

pub(super) fn classify_text(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    if s.starts_with('<') && s.trim_end().ends_with('>') {
        Some(format!("[injected context]\n{s}"))
    } else {
        Some(s.to_string())
    }
}

/// Split a format-native content field into IR blocks.
///
/// `tool` is the source agent; unknown part types retire as gaps naming it so
/// a resumed session shows what its new home could not carry over.
pub(super) fn collect_parts(content: &Value, role: Role, tool: Tool, out: &mut Events) {
    match content {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(Text {
                    role,
                    text: s.clone(),
                });
            }
        }
        Value::Array(items) => {
            for it in items {
                match it.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        if let Some(t) = classify_text(it.get("text")) {
                            out.push(Text { role, text: t });
                        }
                    }
                    // claude/pi reasoning. Sources often keep only a signature
                    // (claude redacts the text); that is a gap, not silence.
                    Some("thinking") => {
                        let text = it
                            .get("thinking")
                            .or_else(|| it.get("text"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let signature = it
                            .get("signature")
                            .or_else(|| it.get("thinkingSignature"))
                            .and_then(|s| s.as_str())
                            .map(str::to_string);
                        if !text.trim().is_empty() {
                            out.push(Thinking {
                                text: text.to_string(),
                                signature,
                            });
                        } else if signature.is_some() {
                            out.push(Gap {
                                kind: "thinking".into(),
                                detail: "redacted reasoning not carried over".into(),
                            });
                        }
                    }
                    Some("tool_use") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it.get("input").map(|i| i.to_string()).unwrap_or_default(),
                            call_id: it.get("id").and_then(|i| i.as_str()).map(str::to_string),
                            status: None,
                        });
                    }
                    // pi/omp assistant turns pair `toolCall` items with
                    // `toolResult`-role messages (named via toolName).
                    Some("toolCall") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            args: it
                                .get("arguments")
                                .map(|i| i.to_string())
                                .unwrap_or_default(),
                            call_id: it.get("id").and_then(|i| i.as_str()).map(str::to_string),
                            status: None,
                        });
                    }
                    // claude tool results: string or [text|image|tool_reference]
                    Some("tool_result") => {
                        let content = it.get("content").unwrap_or(&Value::Null);
                        let call_id = it
                            .get("tool_use_id")
                            .and_then(|c| c.as_str())
                            .map(str::to_string);
                        let is_error = it
                            .get("is_error")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false);
                        if let Some(items) = content.as_array() {
                            for inner in items {
                                if matches!(
                                    inner.get("type").and_then(|t| t.as_str()),
                                    Some("image" | "input_image")
                                ) {
                                    out.push(image_block(inner, tool));
                                }
                            }
                        }
                        let output = text_of(content).unwrap_or_default();
                        if output.trim().is_empty() {
                            if !content_has_image(content) {
                                out.push(tool_output_gap("tool"));
                            }
                        } else {
                            out.push(ToolResult {
                                name: "tool".into(),
                                output,
                                call_id,
                                is_error,
                                duration_ms: None,
                            });
                        }
                    }
                    Some("function_call") => {
                        out.push(ToolCall {
                            name: it
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("function")
                                .to_string(),
                            args: it
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("")
                                .to_string(),
                            call_id: it
                                .get("call_id")
                                .or_else(|| it.get("id"))
                                .and_then(|c| c.as_str())
                                .map(str::to_string),
                            status: it
                                .get("status")
                                .and_then(|s| s.as_str())
                                .map(str::to_string),
                        });
                    }
                    Some("image" | "input_image") => out.push(image_block(it, tool)),
                    None => {
                        if let Some(s) = it.as_str() {
                            out.push(Text {
                                role,
                                text: s.to_string(),
                            });
                        }
                    }
                    Some(other) => out.push(Gap {
                        kind: format!("{}-part", tool.as_str()),
                        detail: format!("{other} part not carried over"),
                    }),
                }
            }
        }
        _ => {}
    }
}
