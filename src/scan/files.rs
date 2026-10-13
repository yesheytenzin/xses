//! Incremental JSONL summaries. Only complete records are checkpointed; a
//! trailing partial line is retried when its writer appends the remainder.
use super::{display_title, looks_like_error, marker_tool, noise_reason, prefixed_title_source};
use crate::{
    extract::text_of,
    model::{Session, Tool},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const GUARD_BYTES: u64 = 256;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    entries: BTreeMap<PathBuf, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    offset: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    state: State,
    visible: State,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Stamp {
    mtime: DateTime<Utc>,
    created: DateTime<Utc>,
    size: u64,
    identity: String,
}
impl Stamp {
    fn read(path: &Path) -> Result<Self> {
        let md = fs::metadata(path)?;
        let mtime = md.modified()?.into();
        let created = md.created().map(DateTime::<Utc>::from).unwrap_or(mtime);
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            format!("{}:{}", md.dev(), md.ino())
        };
        #[cfg(not(unix))]
        let identity = created.to_rfc3339();
        Ok(Self {
            mtime,
            created,
            size: md.len(),
            identity,
        })
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    id: String,
    cwd: PathBuf,
    title: Option<String>,
    first_user: Option<String>,
    messages: u64,
    users: u64,
    assistants: u64,
    failed: u64,
    imported: bool,
    source: Option<Tool>,
}

impl State {
    fn content(
        &mut self,
        role: &str,
        content: Option<&Value>,
        failed: bool,
        skip_tagged_user: bool,
    ) {
        if matches!(role, "user" | "assistant" | "system" | "developer") {
            self.messages += 1;
        }
        let text = content.and_then(text_of).filter(|s| !s.trim().is_empty());
        match (role, text) {
            ("user", Some(text)) => {
                self.users += 1;
                if self.first_user.is_none() && (!skip_tagged_user || !text.starts_with('<')) {
                    self.first_user = Some(crate::text::shorten(&text, 120));
                }
            }
            ("assistant", Some(text)) => {
                self.assistants += 1;
                if failed || looks_like_error(&text) {
                    self.failed += 1;
                }
            }
            _ => {}
        }
    }

    fn record(&mut self, tool: Tool, v: &Value) {
        match tool {
            Tool::Claude => self.claude(v),
            Tool::Codex => self.codex(v),
            Tool::Pi | Tool::Omp => self.pi(v),
            Tool::Opencode => unreachable!(),
        }
    }

    fn claude(&mut self, v: &Value) {
        if self.cwd.as_os_str().is_empty() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                self.cwd = c.into();
            }
        }
        self.imported |= v.get("xses").is_some() || v.get("sessionizer").is_some();
        self.source = marker_tool(v, "/xses/source")
            .or_else(|| marker_tool(v, "/sessionizer/source"))
            .or(self.source);
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(kind, "summary" | "last-prompt") {
            if let Some(id) = v
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.id = id.into();
            }
        }
        if kind == "user" && v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        self.content(
            kind,
            v.pointer("/message/content"),
            v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true),
            true,
        );
    }

    fn codex(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                self.id = v
                    .pointer("/payload/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                self.cwd = v
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                self.imported |= matches!(
                    v.pointer("/payload/cli_version").and_then(Value::as_str),
                    Some("0.0.0-xses" | "0.0.0-sessionizer")
                );
                self.source = marker_tool(v, "/payload/xses/source")
                    .or_else(|| marker_tool(v, "/payload/sessionizer/source"));
            }
            Some("response_item")
                if v.pointer("/payload/type").and_then(Value::as_str) == Some("message") =>
            {
                self.content(
                    v.pointer("/payload/role")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    v.pointer("/payload/content"),
                    v.pointer("/payload/isApiErrorMessage")
                        .and_then(Value::as_bool)
                        == Some(true),
                    false,
                );
            }
            _ => {}
        }
    }

    fn pi(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                self.id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                self.cwd = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                self.imported |= v.get("xses").is_some() || v.get("sessionizer").is_some();
                self.source = marker_tool(v, "/xses/source")
                    .or_else(|| marker_tool(v, "/sessionizer/source"))
                    .or(self.source);
            }
            Some("title") => {
                if let Some(title) = v
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.title = Some(crate::text::shorten(title, 120));
                    if matches!(
                        v.get("source").and_then(Value::as_str),
                        Some("xses" | "sessionizer")
                    ) {
                        self.imported = true;
                        self.source = self.source.or_else(|| prefixed_title_source(title));
                    }
                }
            }
            Some("message") => self.content(
                v.pointer("/message/role")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                v.pointer("/message/content"),
                matches!(
                    v.pointer("/message/stopReason").and_then(Value::as_str),
                    Some("error" | "aborted" | "signal")
                ),
                false,
            ),
            _ => {}
        }
    }

    fn session(&self, tool: Tool, path: &Path, stamp: &Stamp) -> Option<Session> {
        if self.id.is_empty() {
            return None;
        }
        Some(Session {
            id: self.id.clone(),
            tool,
            cwd: self.cwd.clone(),
            mtime: stamp.mtime,
            created: stamp.created,
            size: stamp.size,
            revision: None,
            messages: self.messages,
            summary: display_title(
                self.source.unwrap_or(tool),
                self.title
                    .as_deref()
                    .or(self.first_user.as_deref())
                    .unwrap_or_default(),
            ),
            path: path.into(),
            is_import: self.imported,
            noise: noise_reason(self.users, self.assistants, self.failed),
        })
    }
}

fn bytes_at(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn summarize(path: &Path, tool: Tool, stamp: Stamp, previous: Option<&Entry>) -> Result<Entry> {
    let mut file = File::open(path)?;
    let mut state = State::default();
    if tool == Tool::Claude {
        state.id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into();
    }
    let mut offset = 0;
    if let Some(prev) = previous {
        // These stores append to transcripts. Identity, a prefix and the last
        // checkpoint's boundary guard against replacement, truncation and
        // common rewrites. Same-size edits always force a full parse.
        if stamp.identity == prev.stamp.identity
            && stamp.size > prev.stamp.size
            && bytes_at(&mut file, 0, prev.head.len() as u64)? == prev.head
            && bytes_at(
                &mut file,
                prev.offset.saturating_sub(GUARD_BYTES),
                prev.offset.min(GUARD_BYTES),
            )? == prev.tail
        {
            offset = prev.offset;
            state = prev.state.clone();
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(stamp.size.saturating_sub(offset)));
    let mut line = String::new();
    let mut last = None;
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with('\n') {
            last = serde_json::from_str::<Value>(&line).ok();
            break;
        }
        if let Ok(v) = serde_json::from_str(&line) {
            state.record(tool, &v);
        }
        offset += n as u64;
    }
    let mut visible = state.clone();
    if let Some(last) = last {
        visible.record(tool, &last);
    }
    let mut file = reader.into_inner().into_inner();
    let head = bytes_at(&mut file, 0, offset.min(GUARD_BYTES))?;
    let tail = bytes_at(
        &mut file,
        offset.saturating_sub(GUARD_BYTES),
        offset.min(GUARD_BYTES),
    )?;
    Ok(Entry {
        stamp,
        offset,
        head,
        tail,
        state,
        visible,
    })
}

pub(super) fn scan(tool: Tool, save_cache: bool) -> Result<Vec<Session>> {
    let home = dirs::home_dir().context("no home dir")?;
    let (base, depth) = match tool {
        Tool::Claude => (".claude/projects", 0),
        Tool::Codex => (".codex/sessions", 4),
        Tool::Pi => (".pi/agent/sessions", 2),
        Tool::Omp => (".omp/agent/sessions", 2),
        Tool::Opencode => unreachable!(),
    };
    let root = home.join(base);
    let cache_path = dirs::data_dir()
        .context("no data dir")?
        .join(format!("xses/scan-cache-{}.json", tool.as_str()));
    // A concurrent scanner can proceed read-only. Holding this lock from load
    // through save prevents an older scan from overwriting a newer checkpoint.
    let lock = if save_cache {
        crate::storage::Lock::try_acquire(&cache_path.with_extension("lock"))
            .ok()
            .flatten()
    } else {
        None
    };
    let mut cache: Cache = fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &Cache| c.version == VERSION)
        .unwrap_or_default();
    let mut dirty = cache.version != VERSION;
    cache.version = VERSION;
    let mut entries = BTreeMap::new();
    let mut sessions = Vec::new();
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(&root)
            .min_depth(depth)
            .into_iter()
            .filter_entry(|e| !(tool == Tool::Claude && e.file_name() == "subagents"))
        {
            let entry = entry.with_context(|| format!("scan {}", root.display()))?;
            let path = entry.path();
            if !entry.file_type().is_file()
                || path.extension().and_then(|s| s.to_str()) != Some("jsonl")
            {
                continue;
            }
            if tool == Tool::Codex && !entry.file_name().to_string_lossy().starts_with("rollout-") {
                continue;
            }
            let stamp = match Stamp::read(path) {
                Ok(stamp) => stamp,
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            };
            let prev = cache.entries.remove(path);
            let current = match prev {
                Some(prev) if prev.stamp == stamp => prev,
                previous => {
                    dirty = true;
                    summarize(path, tool, stamp, previous.as_ref())?
                }
            };
            if let Some(session) = current.visible.session(tool, path, &current.stamp) {
                sessions.push(session);
            }
            entries.insert(path.to_owned(), current);
        }
    }
    dirty |= !cache.entries.is_empty();
    if dirty && lock.is_some() {
        cache.entries = entries;
        crate::storage::atomic_write(&cache_path, &serde_json::to_vec(&cache)?).ok();
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("xses-scan-{}-{name}.jsonl", std::process::id()));
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                json!({"type":"session","id":"source","cwd":"/work"}),
                json!({"type":"message","message":{"role":"user","content":"hello"}})
            ),
        )
        .unwrap();
        path
    }
    fn summarize_now(path: &Path, old: Option<&Entry>) -> Entry {
        summarize(path, Tool::Pi, Stamp::read(path).unwrap(), old).unwrap()
    }

    #[test]
    fn appended_records_match_a_full_scan_and_partial_lines_are_retried() {
        let path = fixture("append");
        let before = summarize_now(&path, None);
        assert_eq!(before.state.users, 1);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"message\":")
            .unwrap();
        let partial = summarize_now(&path, Some(&before));
        assert_eq!(partial.offset, before.offset);
        file.write_all(b"{\"role\":\"assistant\",\"content\":\"done\"}}\n")
            .unwrap();
        let appended = summarize_now(&path, Some(&partial));
        let fresh = summarize_now(&path, None);
        assert_eq!(appended.state.assistants, 1);
        assert_eq!(
            serde_json::to_value(&appended.state).unwrap(),
            serde_json::to_value(&fresh.state).unwrap()
        );
        assert_eq!(appended.offset, fs::metadata(&path).unwrap().len());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rewrites_truncation_and_replacement_discard_the_checkpoint() {
        let path = fixture("rewrite");
        let original = summarize_now(&path, None);
        let text = fs::read_to_string(&path).unwrap().replace("hello", "world");
        fs::write(&path, text).unwrap();
        let rewritten = summarize_now(&path, Some(&original));
        assert_eq!(rewritten.state.first_user.as_deref(), Some("world"));
        fs::write(&path, "{}\n").unwrap();
        assert_eq!(summarize_now(&path, Some(&rewritten)).state.users, 0);
        fs::remove_file(&path).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session","id":"replacement","cwd":"/work","padding":"x".repeat(500)})
            ),
        )
        .unwrap();
        let replaced = summarize_now(&path, Some(&original));
        assert_eq!(replaced.state.id, "replacement");
        assert_eq!(replaced.state.users, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn adapters_keep_native_markers_and_noise() {
        let mut claude = State::default();
        claude.record(
            Tool::Claude,
            &json!({"type":"user","isMeta":true,"cwd":"/work","message":{"content":"context"}}),
        );
        assert_eq!(claude.users, 0);
        claude.record(
            Tool::Claude,
            &json!({"type":"last-prompt","sessionId":"c","xses":{"source":"pi","srcId":"p"}}),
        );
        assert!(claude.imported);
        assert_eq!(claude.source, Some(Tool::Pi));
        let mut codex = State::default();
        codex.record(Tool::Codex, &json!({"type":"session_meta","payload":{"id":"x","cwd":"/work","cli_version":"0.0.0-xses"}}));
        codex.record(Tool::Codex, &json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"API Error: failed"}]}}));
        assert!(codex.imported);
        assert_eq!(
            noise_reason(codex.users, codex.assistants, codex.failed).as_deref(),
            Some("failed")
        );
    }
}
