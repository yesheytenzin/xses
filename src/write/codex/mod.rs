//! codex writer: rollout file + `threads` row + history projections.
//!
//! The rollout carries codex's own item vocabulary — `message` with
//! `input_text`/`output_text`/`input_image`, `reasoning`, `function_call`,
//! `function_call_output`, `custom_tool_call(_output)`, and `token_count`
//! events — while the TUI projection (`thread_items`) uses the item types the
//! picker renders: `userMessage`, `agentMessage`, `commandExecution`,
//! `contextCompaction`.
use super::{iso, iso_stamp, manifest, push, synth_id};
use crate::canonical::CanonicalSession;
use serde_json::{json, Value};
use std::path::PathBuf;

/// One rollout record plus (optionally) the projection row that mirrors it.
struct Item {
    /// Wrapper record type in the rollout: `response_item` or `event_msg`.
    wrapper: &'static str,
    payload: Value,
    /// `(thread_items.item_type, item_json)`; `None` keeps the item out of the
    /// projection (ordinals may have gaps — the projection index allows it).
    thread: Option<(String, Value)>,
    item_id: String,
    /// The source message's own time, when it recorded one.
    ts: Option<chrono::DateTime<chrono::Utc>>,
}

pub(super) fn write(session: &CanonicalSession, revision: Option<&str>) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sid = synth_id(session.tool, &super::artifact_key(session, revision));
    // codex vets the filename: `rollout-<ts>-<full-uuid>.jsonl` ("canonical
    // rollout filename"), so the uuid must be in it, not a short id.
    let target = home
        .join(".codex/sessions")
        .join(session.created.format("%Y/%m/%d").to_string())
        .join(format!(
            "rollout-{}-{}.jsonl",
            iso_stamp(session.created),
            sid
        ));
    let ts = iso(&session.created);
    if let Err(e) = seed::backup_thread(&home, &sid) {
        super::journal_failure(&sid, &e.to_string());
        return None;
    }
    let items = rollout_items(session);
    let mut buf = Vec::new();
    // codex's thread/read rejects a rollout whose first line isn't a fully
    // populated session_meta ("does not start with session metadata") —
    // verified by replaying resume against drop-in files.
    let mut meta = json!({
        "session_id": sid,
        "id": sid,
        "timestamp": ts,
        "cwd": session.cwd,
        "runtime_workspace_roots": [session.cwd],
        "originator": "codex-tui",
        "cli_version": "0.0.0-xses",
        "source": "cli",
        "thread_source": "user",
        "model_provider": "openai",
        "history_mode": "paginated",
        "base_instructions": {"text": "Imported by xses."},
        "xses": manifest(session),
        // NOTE: `git`/`context_window` are omitted on purpose — an empty
        // `context_window: {}` fails thread/read deserialization (verified).
    });
    // impersonate the local account so thread/read's ownership checks pass
    if let Some((uid, aid)) = local_creator_ids() {
        meta["creator_user_id"] = json!(uid);
        meta["creator_account_id"] = json!(aid);
    }
    push(
        &mut buf,
        &json!({ "ordinal": 0, "type": "session_meta", "payload": meta, "timestamp": ts }),
    );
    // turn_context carries the workspace the TUI validates on resume
    push(
        &mut buf,
        &json!({
            "ordinal": 1,
            "type": "turn_context",
            "timestamp": ts,
            "payload": {
                "cwd": session.cwd,
                "approval_policy": "never",
                "sandbox_policy": {"type": "disabled"},
                "model": "imported",
            }
        }),
    );
    for (i, item) in items.iter().enumerate() {
        // Each record carries the source message's own time when there is one,
        // so a resumed session shows the timeline it actually had.
        let item_ts = item.ts.map(|t| iso(&t));
        push(
            &mut buf,
            &json!({
                "ordinal": i + 2,
                "type": item.wrapper,
                "payload": item.payload,
                "timestamp": item_ts.unwrap_or_else(|| ts.clone()),
            }),
        );
    }
    if !super::atomic_write_journaled(
        &target,
        &buf,
        &format!("{} {}", session.tool.as_str(), session.id),
    ) {
        return None;
    }
    // codex's own picker lists from the `threads` table, not the sessions dir:
    // register the drop-in file so `codex resume` sees it.
    let title = super::title_for(session);
    if !register_codex_thread(&home, &sid, &target, session, &title) {
        return None;
    }
    // the TUI transcript renders from `thread_history_1.sqlite` projections,
    // never built for drop-in rollouts — seed them so resume shows content.
    if !seed_codex_history(&home, &sid, &items, session, buf.len() as u64) {
        return None;
    }
    Some(target)
}

mod roller;
mod rollout;
mod seed;
mod tools;

use rollout::rollout_items;
pub(crate) use seed::registered;
pub(crate) use seed::retire_codex;
use seed::{local_creator_ids, register_codex_thread, seed_codex_history};
