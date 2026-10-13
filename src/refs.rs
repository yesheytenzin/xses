//! Resolve a source store reference to a `CanonicalSession`.
//!
//! Accepts: `latest`, `latest:N`, tool name, tool:id, tool:last, or a session
//! short-id prefix. Falls back to scanning.
#![allow(clippy::items_after_test_module)]
use crate::canonical::CanonicalSession;
use crate::model::{Session, Tool};
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub enum Ref {
    Latest,
    LatestN(usize),
    Id(String),
}

/// Parse a showagent-style reference: `latest`, `latest:3`, or an id.
pub fn parse_ref(s: &str) -> Option<Ref> {
    let s = s.trim();
    if s == "latest" {
        return Some(Ref::Latest);
    }
    if let Some(rest) = s.strip_prefix("latest:") {
        return rest
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(Ref::LatestN);
    }
    if s.is_empty() {
        None
    } else {
        Some(Ref::Id(s.to_string()))
    }
}

/// Destination kind for a `sync` item: a tool store or a trace format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Tool(Tool),
    Format(&'static str),
}

/// Parse a bare token: tool name, trace format, or nil.
fn parse_dest_token(s: &str) -> Option<Dest> {
    if let Some(t) = Tool::parse(s.trim()) {
        return Some(Dest::Tool(t));
    }
    match s.trim().to_ascii_lowercase().as_str() {
        "sharegpt" => Some(Dest::Format("sharegpt")),
        "openai" => Some(Dest::Format("openai")),
        _ => None,
    }
}

/// Resolved `sync` inputs: explicit refs, `--from` tool filters, destinations.
#[derive(Debug, Default)]
pub struct Plan {
    pub refs: Vec<Ref>,
    pub from_tools: Vec<Tool>,
    pub dests: Vec<Dest>,
}

/// Resolve free-form `sync` items into their plan.
///
/// Rules: tokens parse as tool/format first, else session ref; `from` treats
/// tool names as source filters and anything else as a ref; `to` forces
/// destination interpretation; empty input means all sources.
pub fn resolve_items(items: &[String], from: &[String], to: &[String]) -> Result<Plan> {
    let mut plan = Plan::default();
    for f in from {
        if let Some(t) = Tool::parse(f) {
            if !plan.from_tools.contains(&t) {
                plan.from_tools.push(t);
            }
            continue;
        }
        plan.refs
            .push(parse_ref(f).ok_or_else(|| anyhow::anyhow!("bad session ref {f:?}"))?);
    }
    for t in to {
        plan.dests.push(parse_dest_token(t).ok_or_else(|| {
            anyhow::anyhow!("bad destination {t:?} (want a tool or sharegpt|openai)")
        })?);
    }
    for item in items {
        match parse_dest_token(item) {
            Some(d) => plan.dests.push(d),
            None => plan
                .refs
                .push(parse_ref(item).ok_or_else(|| anyhow::anyhow!("bad session ref {item:?}"))?),
        }
    }
    Ok(plan)
}

/// Tool targets for a sync, given resolved destinations and the source set.
/// No destinations → all writable tools (old `sync` default). Sources are
/// never synced into their own tool (handled per-source at write time).
pub fn tool_targets(dests: &[Dest]) -> Vec<Tool> {
    let mut out: Vec<Tool> = dests
        .iter()
        .filter_map(|d| match d {
            Dest::Tool(t) => Some(*t),
            Dest::Format(_) => None,
        })
        .collect();
    out.dedup();
    out
}

pub fn format_targets(dests: &[Dest]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = dests
        .iter()
        .filter_map(|d| match d {
            Dest::Format(f) => Some(*f),
            Dest::Tool(_) => None,
        })
        .collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(items: &[&str], from: &[&str], to: &[&str]) -> (usize, Vec<Dest>) {
        let plan = resolve_items(
            &items.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &from.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &to.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .expect("resolve");
        (plan.refs.len(), plan.dests)
    }

    fn from_tools(from: &[&str]) -> Vec<Tool> {
        resolve_items(
            &[],
            &from.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
        )
        .expect("resolve")
        .from_tools
    }

    #[test]
    fn empty_means_all() {
        assert_eq!(r(&[], &[], &[]), (0, vec![]));
    }

    #[test]
    fn tools_parse_as_dests() {
        let (n, d) = r(&["claude"], &[], &[]);
        assert_eq!(n, 0);
        assert_eq!(d, vec![Dest::Tool(Tool::Claude)]);
    }

    #[test]
    fn formats_parse_as_dests() {
        let (n, d) = r(&["sharegpt"], &[], &[]);
        assert_eq!(n, 0);
        assert_eq!(d, vec![Dest::Format("sharegpt")]);
    }

    #[test]
    fn ids_parse_as_refs() {
        let (n, d) = r(&["01a12ba6", "latest"], &[], &[]);
        assert_eq!(n, 2);
        assert!(d.is_empty());
    }

    #[test]
    fn mixed_items_split() {
        let (n, d) = r(&["latest", "pi", "claude"], &[], &[]);
        assert_eq!(n, 1);
        assert_eq!(d, vec![Dest::Tool(Tool::Pi), Dest::Tool(Tool::Claude)]);
    }

    #[test]
    fn from_forces_ref() {
        let (n, d) = r(&[], &["pi9f"], &[]);
        assert_eq!(n, 1);
        assert!(d.is_empty());
    }

    #[test]
    fn from_tool_name_filters_sources() {
        assert_eq!(from_tools(&["claude"]), vec![Tool::Claude]);
        assert_eq!(from_tools(&["claude", "pi"]), vec![Tool::Claude, Tool::Pi]);
        assert_eq!(from_tools(&["claude", "claude"]), vec![Tool::Claude]);
        // ids stay refs, tools stay filters
        let plan = resolve_items(&[], &["claude".into(), "01a12ba6".into()], &[]).unwrap();
        assert_eq!(plan.from_tools, vec![Tool::Claude]);
        assert_eq!(plan.refs.len(), 1);
    }

    #[test]
    fn to_forces_dest() {
        let (n, d) = r(&[], &[], &["openai"]);
        assert_eq!(n, 0);
        assert_eq!(d, vec![Dest::Format("openai")]);
    }

    #[test]
    fn bad_to_rejected() {
        assert!(resolve_items(&[], &[], &["nonsense".to_string()]).is_err());
    }

    #[test]
    fn bad_from_rejected() {
        assert!(resolve_items(&[], &["".to_string()], &[]).is_err());
    }

    #[test]
    fn target_partitioning() {
        let dests = vec![Dest::Tool(Tool::Pi), Dest::Format("openai")];
        assert_eq!(tool_targets(&dests), vec![Tool::Pi]);
        assert_eq!(format_targets(&dests), vec!["openai"]);
        assert!(tool_targets(&[]).is_empty());
    }

    #[test]
    fn latest_zero_is_rejected() {
        assert!(parse_ref("latest:0").is_none());
    }
}

/// Turn a discovered session into its IR.
pub fn canonicalize(s: &Session, _path: &Path) -> Result<CanonicalSession> {
    crate::extract::extract(s.tool, &s.id, &s.cwd, s.created, &s.path)
}

/// Pick a session from the discovered list by ref.
pub fn select(sessions: &[Session], r: Ref) -> anyhow::Result<&Session> {
    match r {
        Ref::Latest => {
            let s: anyhow::Result<&Session> = Ok(sessions
                .iter()
                .max_by_key(|s| s.mtime)
                .context("no sessions")?);
            s
        }
        Ref::LatestN(n) => {
            if n == 0 {
                anyhow::bail!("latest:N requires N >= 1");
            }
            let mut sorted: Vec<&Session> = sessions.iter().collect();
            sorted.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            sorted
                .get(n.saturating_sub(1))
                .copied()
                .with_context(|| format!("only {} sessions, asked for latest:{n}", sorted.len()))
        }
        Ref::Id(id) => {
            let matches: Vec<&Session> =
                sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
            match matches.len() {
                1 => Ok(matches[0]),
                0 => anyhow::bail!("no session matching {id:?}"),
                n => anyhow::bail!("ambiguous ref {id:?}: {n} matches"),
            }
        }
    }
    .context("no session selected")
}
