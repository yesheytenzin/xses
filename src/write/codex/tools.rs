//! codex projection tool types: how a tool call renders in the TUI.
//!
//! codex only renders a shell string for `commandExecution`; edits, MCP
//! calls and image views have their own item types.
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone)]
/// How a tool call shows up in codex's own transcript projection. codex only
/// renders a shell string for `commandExecution`; edits, MCP calls and image
/// views have their own item types, which is what its TUI draws.
pub(super) enum ThreadTool {
    Shell(String),
    Read(PathBuf),
    FileChange(Vec<Value>),
    Mcp { server: String, tool: String },
    ImageView(PathBuf),
}

pub(super) fn classify_tool(name: &str, args: &str) -> ThreadTool {
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or(("mcp", rest));
        return ThreadTool::Mcp {
            server: name
                .get("mcp__".len()..)
                .and_then(|r| r.split_once("__").map(|(s, _)| s))
                .unwrap_or(server)
                .to_string(),
            tool: name
                .rsplit_once("__")
                .map(|(_, t)| t)
                .unwrap_or(tool)
                .to_string(),
        };
    }
    let parsed: Option<Value> = serde_json::from_str(args).ok();
    let field = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            parsed
                .as_ref()
                .and_then(|v| v.get(*k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
    };
    if matches!(lower.as_str(), "read" | "read_file" | "readfile") {
        if let Some(path) = field(&["path", "file_path", "filePath"]) {
            return ThreadTool::Read(PathBuf::from(path));
        }
    }
    if matches!(
        lower.as_str(),
        "view_image" | "viewimage" | "image_view" | "read_image"
    ) {
        if let Some(path) = field(&["file_path", "path", "filePath", "filename"]) {
            return ThreadTool::ImageView(PathBuf::from(path));
        }
    }
    if lower == "apply_patch" || lower == "patch" {
        let text = field(&["patch", "input", "content"]).unwrap_or_else(|| args.to_string());
        let changes = patch_changes(&text);
        if !changes.is_empty() {
            return ThreadTool::FileChange(changes);
        }
    }
    if matches!(
        lower.as_str(),
        "write"
            | "create_file"
            | "save_file"
            | "edit"
            | "multiedit"
            | "str_replace"
            | "str_replace_editor"
            | "notebookedit"
            | "notebook_edit"
    ) {
        if let Some(path) = field(&["file_path", "path", "filePath"]) {
            let diff = field(&["content", "new_string", "new_str"]).unwrap_or_default();
            // Whether the file was created or rewritten is not knowable from
            // the call alone, so the change is an update; codex's own patches
            // report `add` only because it applies them itself.
            return ThreadTool::FileChange(vec![json!({
                "path": path,
                "kind": {"type": "update"},
                "diff": diff,
            })]);
        }
    }
    ThreadTool::Shell(shell_command(args, name))
}

pub(super) fn patch_changes(text: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    let flush = |out: &mut Vec<Value>, cur: Option<(String, String, Vec<String>)>| {
        if let Some((path, kind, lines)) = cur {
            // `Update` carries a required-or-null `movePath`; sibling
            // variants ignore the extra key (no deny_unknown_fields).
            out.push(json!({
                "path": path,
                "kind": {"type": kind, "movePath": null},
                "diff": lines.join("\n"),
            }));
        }
    };
    for line in text.lines() {
        let marker = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|m| line.strip_prefix(m).map(|rest| (*m, rest.trim())));
        match marker {
            Some((m, path)) => {
                flush(&mut out, current.take());
                let kind = match m {
                    "*** Add File: " => "add",
                    "*** Delete File: " => "delete",
                    _ => "update",
                };
                current = Some((path.to_string(), kind.to_string(), Vec::new()));
            }
            None => {
                if let Some((_, _, lines)) = current.as_mut() {
                    lines.push(line.to_string());
                }
            }
        }
    }
    flush(&mut out, current.take());
    out
}

/// The command a shell-ish tool actually ran, for the projection row.
pub(super) fn shell_command(args: &str, fallback: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(args) {
        for key in ["command", "cmd", "script", "code"] {
            if let Some(s) = v.get(key).and_then(|c| c.as_str()) {
                return s.to_string();
            }
        }
    }
    if args.trim().is_empty() {
        fallback.to_string()
    } else {
        args.to_string()
    }
}
