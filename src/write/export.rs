//! Trace export: IR -> sharegpt / openai JSONL.
use super::role_str;
use crate::canonical::CanonicalSession;
use anyhow::Result;
use serde_json::{json, Value};
/// Export the IR into portable interchange formats (unisessions-style traces).
pub(crate) fn export_trace(session: &CanonicalSession, format: &str) -> Result<Vec<u8>> {
    let (messages, _report) = session.messages();
    match format {
        "sharegpt" => {
            let rows: Vec<Value> = messages
                .iter()
                .map(|(role, text)| json!({"from": role_str(*role), "value": text}))
                .collect();
            Ok(serde_json::to_vec_pretty(&json!({"conversations": rows}))?)
        }
        "openai" => {
            let rows: Vec<Value> = messages
                .iter()
                .map(|(role, text)| json!({"role": role_str(*role), "content": text}))
                .collect();
            Ok(serde_json::to_vec_pretty(&json!({"messages": rows}))?)
        }
        other => anyhow::bail!("unknown trace format {other:?} (want sharegpt|openai)"),
    }
}
