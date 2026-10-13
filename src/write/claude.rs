//! claude writer: transcript + `custom-title.json` sidecar.
use super::{atomic_write, first_ask, iso, push, role_str, synth_id};
use crate::canonical::CanonicalSession;
use serde_json::json;
use std::path::PathBuf;
pub(super) fn write(
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Vec<Option<PathBuf>> {
    let Some((rollout, sidecar)) = write_claude_inner(session, msgs) else {
        return vec![None];
    };
    // The sidecar is a second artifact clean() must remove.
    vec![Some(rollout), Some(sidecar)]
}
fn write_claude_inner(
    session: &CanonicalSession,
    msgs: &[(crate::canonical::Role, String)],
) -> Option<(PathBuf, PathBuf)> {
    let home = dirs::home_dir()?;
    let slug = session.cwd.to_string_lossy().replace('/', "-");
    let sid = synth_id(session.tool, &session.id);
    // Claude's /resume only lists files whose name == the session UUID. Its row
    // label is `agentName || customTitle || aiTitle || summary || firstPrompt`
    // (binary-verified): firstPrompt is the bare first user message, so the
    // [agent] prefix must live in the customTitle sidecar:
    // `<slug>/<sid>/custom-title.json` = `{"customTitle": ...}`.
    let title = format!("[{}] {}", session.tool.as_str(), first_ask(msgs));
    let target = home
        .join(".claude/projects")
        .join(slug)
        .join(format!("{sid}.jsonl"));
    let mut buf = Vec::new();

    // Real uuids: the picker's leafUuid/parent chains must be UUID-shaped.
    let mut uuids: Vec<String> = Vec::with_capacity(msgs.len());
    for i in 0..msgs.len() {
        uuids.push(uuid_like(&sid, i));
    }
    push(
        &mut buf,
        &json!({
            "type": "last-prompt",
            "lastPrompt": title,
            "leafUuid": uuids.last().cloned().unwrap_or_else(|| sid.clone()),
            "sessionId": sid,
            "xses": {"source": session.tool.as_str(), "srcId": session.id},
        }),
    );
    let mut parent: Option<String> = None;
    for (i, (role, text)) in msgs.iter().enumerate() {
        let uuid = uuids[i].clone();
        push(
            &mut buf,
            &json!({
                "type": role_str(*role),
                "message": {"role": role_str(*role), "content": text},
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": iso(&session.created),
                "cwd": session.cwd,
                "sessionId": sid,
                "isSidechain": false,
                "userType": "external",
            }),
        );
        parent = Some(uuid);
    }
    atomic_write(&target, &buf).ok()?;
    // Sidecar must exist or the picker row shows the bare first prompt.
    let sidecar_dir = target.with_extension("");
    std::fs::create_dir_all(&sidecar_dir).ok()?;
    let sidecar = sidecar_dir.join("custom-title.json");
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&json!({"customTitle": title})).ok()?,
    )
    .ok()?;
    Some((target, sidecar))
}
/// Deterministic UUID-shaped id derived from the session id + index.
fn uuid_like(seed: &str, i: usize) -> String {
    let mut h = [0x811c9dc5u32; 4];
    let key = format!("{seed}:{i}");
    for (idx, b) in key.as_bytes().iter().enumerate() {
        let b = *b as u32;
        h[idx % 4] ^= b;
        h[idx % 4] = h[idx % 4].wrapping_mul(0x01000193);
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
