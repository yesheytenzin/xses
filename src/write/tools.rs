//! Translate known tool dialects without guessing at unknown/custom tools.
use crate::canonical::{Block, CanonicalSession, Event};
use crate::model::Tool;
use serde_json::{json, Value};

pub(super) fn native_tool(target: Tool, name: &str, args: &str) -> (String, Value) {
    let mut input = super::args_json(args);
    let lower = name.to_ascii_lowercase();
    let (kind, aliases): (&str, &[(&str, &str)]) = match lower.as_str() {
        "bash" | "shell" | "exec" | "exec_command" | "terminal" | "sh" => {
            ("bash", &[("cmd", "command")])
        }
        "read" | "read_file" | "readfile" => {
            ("read", &[("file_path", "path"), ("filePath", "path")])
        }
        "write" | "write_file" | "writefile" | "create_file" => {
            ("write", &[("file_path", "path"), ("filePath", "path")])
        }
        "edit" | "edit_file" | "str_replace" => (
            "edit",
            &[
                ("file_path", "path"),
                ("filePath", "path"),
                ("old_string", "oldText"),
                ("new_string", "newText"),
            ],
        ),
        _ => return (name.into(), input),
    };
    if !input.is_object() {
        return (name.into(), input);
    }
    for (from, to) in aliases {
        if input.get(to).is_none() {
            if let Some(value) = input.get(from).cloned() {
                input[*to] = value;
                input.as_object_mut().unwrap().remove(*from);
            }
        }
    }
    if target == Tool::Claude {
        for (from, to) in [
            ("path", "file_path"),
            ("oldText", "old_string"),
            ("newText", "new_string"),
        ] {
            if input.get(to).is_none() {
                if let Some(value) = input.as_object_mut().unwrap().remove(from) {
                    input[to] = value;
                }
            }
        }
        let name = match kind {
            "bash" => "Bash",
            "read" => "Read",
            "write" => "Write",
            "edit" => "Edit",
            _ => unreachable!(),
        };
        (name.into(), input)
    } else {
        if target == Tool::Opencode {
            for (from, to) in [
                ("path", "filePath"),
                ("oldText", "oldString"),
                ("newText", "newString"),
            ] {
                if input.get(to).is_none() {
                    if let Some(value) = input.as_object_mut().unwrap().remove(from) {
                        input[to] = value;
                    }
                }
            }
        }
        (kind.into(), input)
    }
}

fn skill_instructions(block: &Block) -> Option<(std::path::PathBuf, &str, Option<&str>)> {
    match block {
        Block::Context {
            kind,
            text: Some(text),
            path,
        } if kind == "meta" || kind == "skill" => {
            let directory = text
                .lines()
                .find_map(|line| line.strip_prefix("Base directory for this skill: "));
            let file = path.clone().or_else(|| {
                directory
                    .filter(|dir| !dir.trim().is_empty())
                    .map(|dir| std::path::PathBuf::from(dir.trim()).join("SKILL.md"))
            })?;
            Some((file, text, None))
        }
        Block::Text {
            role: crate::canonical::Role::User,
            text,
        } => {
            // Match Pi's documented native wrapper exactly; ordinary quoted
            // examples and malformed wrappers stay ordinary user text.
            let header = text.strip_prefix("<skill name=\"")?;
            let (name, rest) = header.split_once("\" location=\"")?;
            let (path, rest) = rest.split_once("\">\n")?;
            if name.is_empty() || path.is_empty() {
                return None;
            }
            let (_, tail) = rest.split_once("\n</skill>")?;
            let prompt = if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix("\n\n")?)
            };
            Some((path.into(), &text[..text.len() - tail.len()], prompt))
        }
        _ => None,
    }
}

/// Claude injects skill instructions as meta user text. Represent a recorded
/// skill load as a completed file read so native UIs can fold its body. Never
/// read the current file: the historical content is the context to preserve.
pub(super) fn skill_history(session: &CanonicalSession) -> std::borrow::Cow<'_, CanonicalSession> {
    if !session
        .events
        .iter()
        .any(|event| skill_instructions(&event.block).is_some())
    {
        return std::borrow::Cow::Borrowed(session);
    }
    let mut converted = CanonicalSession {
        id: session.id.clone(),
        tool: session.tool,
        cwd: session.cwd.clone(),
        created: session.created,
        title: session.title.clone(),
        events: Vec::with_capacity(session.events.len()),
        msg_times: Default::default(),
    };
    let mut msg = 0;
    for (original_msg, blocks) in session.grouped() {
        for block in blocks {
            let skill = skill_instructions(block);
            if let Some((path, text, prompt)) = skill {
                msg += 1;
                let id = format!("xses-skill-{}", super::uuid_like(&session.id, msg as usize));
                converted.events.push(Event {
                    msg,
                    block: Block::ToolCall {
                        name: "read".into(),
                        args: json!({"path":path}).to_string(),
                        call_id: Some(id.clone()),
                        status: Some("completed".into()),
                    },
                });
                if let Some(time) = session.msg_times.get(&original_msg) {
                    converted.msg_times.insert(msg, *time);
                }
                msg += 1;
                converted.events.push(Event {
                    msg,
                    block: Block::ToolResult {
                        name: "read".into(),
                        output: text.to_string(),
                        call_id: Some(id),
                        is_error: false,
                        duration_ms: None,
                    },
                });
                msg += 1;
                if let Some(prompt) = prompt {
                    converted.events.push(Event {
                        msg,
                        block: Block::Text {
                            role: crate::canonical::Role::User,
                            text: prompt.to_string(),
                        },
                    });
                    msg += 1;
                }
            } else {
                converted.events.push(Event {
                    msg,
                    block: block.clone(),
                });
                if let Some(time) = session.msg_times.get(&original_msg) {
                    converted.msg_times.insert(msg, *time);
                }
            }
        }
        msg += 1;
    }
    std::borrow::Cow::Owned(converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn injected_skills_become_paired_reads_without_losing_instructions() {
        let body = "Base directory for this skill: /recorded/skill\n# Historical instructions";
        let source = CanonicalSession {
            id: "skill-fixture".into(),
            tool: Tool::Claude,
            cwd: "/project".into(),
            created: chrono::Utc::now(),
            title: None,
            msg_times: Default::default(),
            events: vec![
                Event {
                    msg: 0,
                    block: Block::Context {
                        kind: "meta".into(),
                        text: Some(body.into()),
                        path: None,
                    },
                },
                Event {
                    msg: 1,
                    block: Block::Context {
                        kind: "meta".into(),
                        text: Some("unrelated context".into()),
                        path: None,
                    },
                },
            ],
        };
        let native = skill_history(&source);
        assert!(
            matches!(&native.events[0].block, Block::ToolCall { args, .. } if args.contains("/recorded/skill/SKILL.md"))
        );
        let Block::ToolCall { call_id: first, .. } = &native.events[0].block else {
            panic!("skill read")
        };
        let Block::ToolResult {
            call_id: second,
            output,
            ..
        } = &native.events[1].block
        else {
            panic!("skill result")
        };
        assert_eq!(first, second);
        assert_eq!(output, body);
        assert!(matches!(&native.events[2].block, Block::Context { .. }));
        assert_eq!(source.events.len(), 2);
    }

    #[test]
    fn pi_wrappers_preserve_the_skill_body_and_separate_user_prompt() {
        let text = "<skill name=\"audit\" location=\"/skills/audit/SKILL.md\">\nFull instructions\n</skill>\n\nPlease audit this project";
        let block = Block::Text {
            role: crate::canonical::Role::User,
            text: text.into(),
        };
        let (path, instructions, prompt) = skill_instructions(&block).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/skills/audit/SKILL.md"));
        assert_eq!(prompt, Some("Please audit this project"));
        assert!(instructions.contains("Full instructions"));
        assert!(!instructions.contains("Please audit"));
        let ordinary = Block::Text {
            role: crate::canonical::Role::User,
            text: "Example: <skill name=\"audit\" location=\"a\">".into(),
        };
        assert!(skill_instructions(&ordinary).is_none());
    }

    #[test]
    fn known_dialects_translate_but_custom_tools_keep_their_arguments() {
        assert_eq!(
            native_tool(Tool::Claude, "exec_command", r#"{"cmd":"pwd"}"#),
            ("Bash".into(), json!({"command":"pwd"}))
        );
        assert_eq!(
            native_tool(Tool::Pi, "Read", r#"{"file_path":"SKILL.md"}"#),
            ("read".into(), json!({"path":"SKILL.md"}))
        );
        assert_eq!(
            native_tool(
                Tool::Omp,
                "Edit",
                r#"{"file_path":"a","old_string":"b","new_string":"c"}"#
            ),
            (
                "edit".into(),
                json!({"path":"a","oldText":"b","newText":"c"})
            )
        );
        assert_eq!(
            native_tool(Tool::Opencode, "mcp__docs__read", r#"{"file_path":"a"}"#),
            ("mcp__docs__read".into(), json!({"file_path":"a"}))
        );
    }
}
