//! Import-matrix coverage (TODO-3): the `--from X --to Y` contract at the
//! idempotency layer, without touching live agent stores.
//!
//! Full end-to-end imports need the agents' CLIs and databases, so this
//! module pins the properties a correct matrix run depends on:
//! deterministic per-pair ids, per-target fingerprints, SQL-safe id
//! handling, and no-op contracts. Test-only: included via
//! `#[cfg(test)] mod matrix` in `main.rs`. A hermetic fixture-store matrix
//! is the explicit next step (see TODO-3).
use std::collections::HashSet;

use crate::model::Tool;
use crate::scan::delete_opencode_sessions;
use crate::sync::fingerprint;
use crate::write::import_id_for;

fn session(tool: Tool, id: &str) -> crate::model::Session {
    crate::model::Session {
        id: id.into(),
        tool,
        cwd: "/tmp/project".into(),
        mtime: chrono::Utc::now(),
        created: chrono::Utc::now(),
        size: 1234,
        revision: None,
        messages: 3,
        summary: "test".into(),
        path: "/tmp/project/session.jsonl".into(),
        is_import: false,
        noise: None,
    }
}

fn is_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Every (source tool, source id) maps to one stable import slug; different
/// sources never collide, and hostile source ids still yield safe slugs
/// (the id is hashed, never interpolated).
#[test]
fn import_matrix_ids_are_deterministic_and_pairwise_distinct() {
    let tools = Tool::all_tools();
    let mut seen = HashSet::new();
    for from in tools {
        for source_id in ["ses_abc123", "01a1349e-571e-727e-b6e4-221162373935", "x"] {
            let id = import_id_for(*from, source_id);
            assert!(is_slug(&id), "{from:?}/{source_id} -> {id:?}");
            assert_eq!(id, import_id_for(*from, source_id), "not deterministic");
            assert!(seen.insert((*from, source_id, id.clone())), "collision");
        }
    }
    // Same native id in two tools must not share an import identity.
    assert_ne!(
        import_id_for(Tool::Claude, "same"),
        import_id_for(Tool::Codex, "same")
    );
    // Hostile source ids hash down to safe slugs.
    for hostile in ["' OR '1'='1", "ses_x'; DROP TABLE session;--", "a b/c:d"] {
        let id = import_id_for(Tool::Omp, hostile);
        assert!(is_slug(&id), "{hostile:?} -> {id:?}");
    }
}

/// One source × every target yields one fingerprint per target: the key
/// carries target, source, and the writer mapping version, so a mapping
/// bump re-imports (upgrades) instead of skipping.
#[test]
fn fingerprints_key_target_source_and_mapping() {
    let src = session(Tool::Omp, "01a1349e");
    let keys: Vec<String> = Tool::all_tools()
        .iter()
        .map(|t| fingerprint(*t, &src, None))
        .collect();
    assert_eq!(keys.len(), Tool::all_tools().len());
    let unique: HashSet<_> = keys.iter().collect();
    assert_eq!(unique.len(), keys.len(), "targets share a key: {keys:?}");
    for (tool, key) in Tool::all_tools().iter().zip(&keys) {
        assert!(
            key.starts_with(&format!("{}|omp|01a1349e|", tool.as_str())),
            "{key}"
        );
        assert!(
            key.contains(&format!("|m{}|", crate::write::MAPPING)),
            "{key}"
        );
    }
    // Scope and revision participate: different sync modes, different keys.
    assert_ne!(
        fingerprint(Tool::Pi, &src, None),
        fingerprint(Tool::Pi, &src, Some(5))
    );
    let mut revised = src.clone();
    revised.revision = Some("digest:abc".into());
    assert_ne!(
        fingerprint(Tool::Pi, &src, None),
        fingerprint(Tool::Pi, &revised, None)
    );
}

/// Two-layer identity: the fingerprint moves with content (mtime/size),
/// while the artifact revision is content-blind — so a live source's
/// updates overwrite the same paths instead of minting new ones.
#[test]
fn identity_is_stable_while_fingerprint_tracks_content() {
    let a = session(Tool::Omp, "01a1349e");
    let mut b = a.clone();
    b.mtime += chrono::Duration::seconds(60);
    b.size += 1;
    assert_ne!(
        fingerprint(Tool::Pi, &a, None),
        fingerprint(Tool::Pi, &b, None),
        "content change must re-import"
    );
    assert_eq!(
        crate::write::artifact_revision(),
        crate::write::artifact_revision(),
        "artifact identity must not depend on content"
    );
    assert!(
        crate::write::artifact_revision().contains(&crate::write::MAPPING.to_string()),
        "identity migrates with the mapping"
    );
    // Same session+target always maps to one import slug.
    assert_eq!(
        import_id_for(Tool::Pi, "01a1349e@m10"),
        import_id_for(Tool::Pi, "01a1349e@m10")
    );
}

/// Failure-mode contracts: hostile ids never reach SQL (rejected at the
/// choke point), and cleaning nothing touches nothing.
#[test]
fn failure_modes_are_typed_not_panics() {
    assert!(crate::proc::sql_quote_id("' OR '1'='1").is_err());
    assert!(crate::proc::sql_quote_id("").is_err());
    assert!(crate::proc::sql_id_list(["ok_1", "no'pe"].into_iter()).is_err());
    let removed = delete_opencode_sessions(&HashSet::new(), &crate::progress::Progress::disabled())
        .expect("empty clean is a no-op");
    assert_eq!(removed, 0);
}
