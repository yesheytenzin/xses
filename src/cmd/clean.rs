//! `xses clean`: drop the newest synced session here, or every import with
//! `--all`. Sourcing the scope from the index keeps it precise: only sessions
//! xses actually wrote are candidates.
use crate::index;
use crate::model::Tool;
use crate::scope;
use crate::sync;
use std::collections::HashSet;

pub fn run(all: bool, dry_run: bool) -> anyhow::Result<()> {
    let removed = if all {
        sync::clean(dry_run)?
    } else {
        let Some(selected) = newest_synced_here()? else {
            println!(
                "{}: nothing synced in this directory",
                if dry_run { "dry-run" } else { "clean" }
            );
            return Ok(());
        };
        sync::clean_selected(dry_run, &selected)?
    };
    println!(
        "{}: removed {} imported artifacts",
        if dry_run { "dry-run" } else { "clean" },
        removed
    );
    Ok(())
}

/// The most recently synced session in the current directory, as recorded by
/// the index (newest record first).
fn newest_synced_here() -> anyhow::Result<Option<HashSet<(Tool, String)>>> {
    let recorded = index::sources_newest_first()?;
    if recorded.is_empty() {
        return Ok(None);
    }
    let sessions = scope::load(false, true)?;
    let here = |tool: Tool, id: &str| {
        sessions
            .iter()
            .any(|s| s.tool == tool && s.id == id && !s.is_import)
    };
    Ok(recorded
        .into_iter()
        .find(|(tool, id)| here(*tool, id))
        .map(|src| HashSet::from([src])))
}
